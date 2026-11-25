//! Linux cgroup memory ceiling for one executor command and its descendants.
use std::{
    fs::{self, File},
    io,
    os::{fd::AsRawFd, unix::process::CommandExt},
    path::PathBuf,
    process::Command,
    sync::{Arc, Mutex, Weak},
};

const MAX_COMMAND_BYTES: u64 = 8 * 1024 * 1024 * 1024;

static EXECUTOR_GROUP: Mutex<Weak<Cgroup>> = Mutex::new(Weak::new());

pub(crate) struct CommandMemory {
    group: Cgroup,
    _executor: Arc<Cgroup>,
}

struct Cgroup(PathBuf);

pub(crate) fn default_ceiling() -> crate::Result<u64> {
    let meminfo = fs::read_to_string("/proc/meminfo")?;
    let bytes = meminfo
        .lines()
        .find_map(|line| line.strip_prefix("MemTotal:"))
        .and_then(|line| line.split_whitespace().next())
        .and_then(|kib| kib.parse::<u64>().ok())
        .and_then(|kib| kib.checked_mul(512))
        .filter(|bytes| *bytes > 0)
        .ok_or("could not determine command memory ceiling")?
        .min(MAX_COMMAND_BYTES);
    round_down_to_page(bytes)
}

fn round_down_to_page(bytes: u64) -> crate::Result<u64> {
    // SAFETY: sysconf reads a scalar system property.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    let page = u64::try_from(page)
        .ok()
        .filter(|page| *page > 0)
        .ok_or("invalid page size")?;
    let limit = bytes / page * page;
    if limit == 0 {
        return Err("executor memory ceiling is below one page".into());
    }
    Ok(limit)
}

impl CommandMemory {
    pub(crate) fn prepare() -> crate::Result<Option<Self>> {
        let limit = default_ceiling()?;
        let current = crate::worker::limits::group_path(&fs::read_to_string("/proc/self/cgroup")?)?;
        // Reuse an equal or stricter inherited boundary, including worker scopes.
        let mut inherited_memory = u64::MAX;
        let mut no_swap = false;
        for group in current
            .ancestors()
            .take_while(|p| p.starts_with("/sys/fs/cgroup"))
        {
            if let Some(bytes) = read_limit(group.join("memory.max"))? {
                inherited_memory = inherited_memory.min(bytes);
            }
            no_swap |= read_limit(group.join("memory.swap.max"))? == Some(0);
        }
        if inherited_memory <= limit && no_swap {
            return Ok(None);
        }

        // A populated leaf cannot enable domain controllers for children. Use a
        // sibling only when leaving the leaf preserves its resource boundaries.
        let controllers = fs::read_to_string(current.join("cgroup.subtree_control"))?;
        let parent = if controllers.split_whitespace().any(|name| name == "memory") {
            current.as_path()
        } else if current
            .file_name()
            .is_some_and(|name| name == "ssmcp-supervisor")
        {
            // The systemd service starts in a dedicated leaf, leaving the
            // delegated unit empty so the memory controller can be enabled.
            let unit = current.parent().ok_or("executor cgroup has no parent")?;
            fs::write(unit.join("cgroup.subtree_control"), b"+memory")?;
            unit
        } else {
            // Leave the ordinary desktop scope only. Other unit types may
            // impose unrelated resource limits that must remain in force.
            if !current
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("app-") && name.ends_with(".scope"))
                || read_limit(current.join("memory.max"))?.is_some()
                || read_limit(current.join("memory.swap.max"))?.is_some()
            {
                return Err("executor needs a delegated cgroup v2 memory controller or an inherited memory and swap ceiling".into());
            }
            current.parent().ok_or("executor cgroup has no parent")?
        };
        // One shared ceiling also covers concurrent commands from this executor.
        let executor = {
            let mut shared = EXECUTOR_GROUP.lock().unwrap();
            if let Some(group) = shared.upgrade() {
                group
            } else {
                let group = Arc::new(Cgroup::create(parent, limit, false)?);
                fs::write(group.0.join("cgroup.subtree_control"), b"+memory")?;
                *shared = Arc::downgrade(&group);
                group
            }
        };
        let group = Cgroup::create(&executor.0, limit, true)?;
        Ok(Some(Self {
            group,
            _executor: executor,
        }))
    }

    pub(crate) fn attach(&self, command: &mut Command) -> io::Result<()> {
        let procs = File::options()
            .write(true)
            .open(self.group.0.join("cgroup.procs"))?;
        // SAFETY: the callback uses only write and errno before exec. Writing
        // zero migrates the calling child, including for a PID namespace.
        unsafe {
            command.pre_exec(move || {
                loop {
                    if libc::write(procs.as_raw_fd(), b"0".as_ptr().cast(), 1) == 1 {
                        return Ok(());
                    }
                    let error = io::Error::last_os_error();
                    if error.kind() != io::ErrorKind::Interrupted {
                        return Err(error);
                    }
                }
            });
        }
        Ok(())
    }
}

impl Cgroup {
    fn create(parent: &std::path::Path, limit: u64, kill_group: bool) -> crate::Result<Self> {
        let mut nonce = [0; 16];
        crate::platform::random_bytes(&mut nonce)?;
        let path = parent.join(format!("ssmcp-exec-{}", crate::hex::encode(nonce)));
        fs::create_dir(&path)?;
        let group = Self(path);
        let limit = round_down_to_page(limit)?;
        let kill_group = u64::from(kill_group);
        fs::write(group.0.join("memory.max"), limit.to_string())?;
        fs::write(group.0.join("memory.swap.max"), b"0")?;
        fs::write(group.0.join("memory.oom.group"), kill_group.to_string())?;
        if read_limit(group.0.join("memory.max"))? != Some(limit)
            || read_limit(group.0.join("memory.swap.max"))? != Some(0)
            || read_limit(group.0.join("memory.oom.group"))? != Some(kill_group)
        {
            return Err("kernel did not install the executor memory ceiling".into());
        }
        Ok(group)
    }
}

impl Drop for Cgroup {
    fn drop(&mut self) {
        if fs::remove_dir(&self.0).is_err() {
            let _ = fs::write(self.0.join("cgroup.kill"), b"1");
            let _ = fs::remove_dir(&self.0);
        }
    }
}

fn read_limit(path: PathBuf) -> io::Result<Option<u64>> {
    match fs::read_to_string(path) {
        Ok(text) if text.trim() == "max" => Ok(None),
        Ok(text) => text.trim().parse().map(Some).map_err(io::Error::other),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_ceiling_applies_to_the_command_and_its_children() {
        use std::os::unix::process::ExitStatusExt;

        let Some(executor) = CommandMemory::prepare().unwrap() else {
            // An already constrained runner cannot be assumed to delegate its
            // controller; the production path inherits that runner's limit.
            return;
        };
        let group = Cgroup::create(&executor._executor.0, 64 * 1024 * 1024, true).unwrap();
        let memory = CommandMemory {
            group,
            _executor: executor._executor.clone(),
        };
        let mut command = Command::new("python3");
        command.args([
            "-c",
            "import os; pid = os.fork(); exec('os.waitpid(pid, 0)' if pid else 'data = bytearray(128 * 1024 * 1024)')",
        ]);
        memory.attach(&mut command).unwrap();
        assert_eq!(command.status().unwrap().signal(), Some(libc::SIGKILL));
        let events = fs::read_to_string(memory.group.0.join("memory.events")).unwrap();
        assert!(
            events.lines().any(|line| line == "oom_group_kill 1"),
            "{events}"
        );
        let path = memory.group.0.clone();
        drop(memory);
        assert!(!path.exists());
    }
}
