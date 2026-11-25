//! Private Linux memory cgroup for one command tree. The pipe guardian stays
//! outside the leaf so it can reap and forward the real command's exit status.
use crate::command_runtime::Driver;
use std::{
    ffi::CString,
    fs::{self, File},
    future::Future,
    io,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{ffi::OsStrExt, process::CommandExt},
    },
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};
use zrun::io::{AsyncFd, Nonblocking};

pub(crate) struct CommandMemory {
    group: Group,
    limit: u64,
    changes: AsyncFd<File>,
}

#[derive(Default)]
pub(crate) struct Receipt {
    pub(crate) exceeded: bool,
    pub(crate) limit: Option<u64>,
    pub(crate) peak: Option<u64>,
    pub(crate) cleanup_error: Option<String>,
}

struct Group(PathBuf);

fn task_budget(physical: u64, session: u64, page: u64) -> crate::Result<u64> {
    let bytes = (physical / 8).min(session / 2);
    if page == 0 || bytes < page {
        return Err("task memory budget is below one kernel page".into());
    }
    Ok(bytes / page * page)
}

fn read_number(group: &Path, name: &str) -> io::Result<u64> {
    fs::read_to_string(group.join(name))?
        .trim()
        .parse()
        .map_err(io::Error::other)
}

fn counter(text: &str, name: &str) -> io::Result<u64> {
    text.lines()
        .find_map(|line| {
            let (key, n) = line.split_once(' ')?;
            (key == name).then(|| n.trim().parse().map_err(io::Error::other))
        })
        .unwrap_or_else(|| Err(io::Error::other(format!("missing cgroup counter {name}"))))
}

fn oom_killed(events: &str) -> io::Result<bool> {
    // oom is charged to the cgroup whose limit caused the OOM. oom_kill alone
    // can count a victim selected by global OOM or by an ancestor's limit.
    let oom = counter(events, "oom")?;
    let killed = counter(events, "oom_kill")?;
    Ok(oom > 0 && killed > 0)
}

fn delegated_unit(current: &Path) -> crate::Result<(&Path, &Path)> {
    let unit = current.parent().ok_or("missing delegated unit")?;
    let session = unit.parent().ok_or("missing session slice")?;
    let valid_slice = session
        .file_name()
        .and_then(|v| v.to_str())
        .and_then(|v| v.strip_prefix("ssmcp-s")?.strip_suffix(".slice"))
        .is_some_and(|id| id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit()));
    if current.file_name().is_none_or(|v| v != "ssmcp-supervisor")
        || !unit
            .file_name()
            .and_then(|v| v.to_str())
            .is_some_and(|v| v.ends_with(".service") || v.ends_with(".scope"))
        || !valid_slice
        || session
            .parent()
            .and_then(Path::file_name)
            .is_none_or(|v| v != "ssmcp.slice")
    {
        return Err(
            "task memory requires a delegated supervisor in the admitted session slice".into(),
        );
    }
    Ok((unit, session))
}

impl CommandMemory {
    pub(crate) fn for_job() -> crate::Result<Option<Self>> {
        let current = crate::worker::limits::group_path(&fs::read_to_string("/proc/self/cgroup")?)?;
        // Standalone library tests construct Shell without starting a client.
        // Real client builds and tests in a session always require admission.
        #[cfg(test)]
        if !current
            .ancestors()
            .any(|p| p.file_name().is_some_and(|n| n == "ssmcp.slice"))
        {
            return Ok(None);
        }
        Self::prepare(&current).map(Some)
    }

    fn prepare(current: &Path) -> crate::Result<Self> {
        let (unit, session) = delegated_unit(current)?;
        if !fs::read_to_string(unit.join("cgroup.subtree_control"))?
            .split_whitespace()
            .any(|v| v == "memory")
        {
            return Err("session unit has not delegated the memory controller".into());
        }
        let mut ceiling = read_number(session, "memory.max")?;
        // The task remains beneath its unit and every enclosing quota. A cap
        // placed on the runner leaf itself must not be left behind.
        if fs::read_to_string(current.join("memory.max"))?.trim() != "max" {
            return Err(
                "cannot move a task out of a separately memory-limited supervisor leaf".into(),
            );
        }
        for parent in unit
            .ancestors()
            .take_while(|p| p.starts_with("/sys/fs/cgroup"))
        {
            match fs::read_to_string(parent.join("memory.max")) {
                Ok(s) if s.trim() != "max" => ceiling = ceiling.min(s.trim().parse()?),
                Ok(_) => (),
                Err(e) if e.kind() == io::ErrorKind::NotFound => (),
                Err(e) => return Err(e.into()),
            }
        }
        let meminfo = fs::read_to_string("/proc/meminfo")?;
        let physical = meminfo
            .lines()
            .find_map(|line| line.strip_prefix("MemTotal:"))
            .and_then(|v| {
                v.split_whitespace()
                    .next()?
                    .parse::<u64>()
                    .ok()?
                    .checked_mul(1024)
            })
            .ok_or("MemTotal unavailable")?;
        // SAFETY: sysconf reads a scalar system property.
        let page = u64::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) })?;
        Self::create(unit, task_budget(physical, ceiling, page)?)
    }

    fn create(parent: &Path, limit: u64) -> crate::Result<Self> {
        let mut nonce = [0; 16];
        crate::platform::random_bytes(&mut nonce)?;
        let path = parent.join(format!("ssmcp-exec-{}", crate::hex::encode(nonce)));
        fs::create_dir(&path)?;
        let group = Group(path);
        for (name, value) in [
            ("memory.max", limit),
            ("memory.swap.max", 0),
            ("memory.oom.group", 1),
        ] {
            fs::write(group.0.join(name), value.to_string())?;
            if read_number(&group.0, name)? != value {
                return Err(format!("kernel did not apply task {name}").into());
            }
        }
        // Install the watch before the first member can enter. The counters in
        // the newly created leaf are monotonic for its entire lifetime.
        let path = group.0.join("memory.events.local");
        let initial = fs::read_to_string(&path)?;
        if counter(&initial, "oom")? != 0 || counter(&initial, "oom_kill")? != 0 {
            return Err("new task cgroup already reports an OOM kill".into());
        }
        File::options()
            .write(true)
            .open(group.0.join("cgroup.kill"))?;
        let path = CString::new(path.as_os_str().as_bytes())?;
        // SAFETY: success transfers an owned, nonblocking, close-on-exec fd.
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error().into());
        }
        let file = unsafe { File::from_raw_fd(fd) };
        // SAFETY: file and the NUL-terminated path are live during the syscall.
        if unsafe { libc::inotify_add_watch(fd, path.as_ptr(), libc::IN_MODIFY) } < 0 {
            return Err(io::Error::last_os_error().into());
        }
        let changes = AsyncFd::new(&Driver::get()?.io, file, Nonblocking::Set)?;
        Ok(Self {
            group,
            limit,
            changes,
        })
    }

    pub(crate) fn initial_receipt(&self) -> Receipt {
        Receipt {
            limit: Some(self.limit),
            ..Receipt::default()
        }
    }

    pub(crate) fn attach(&self, command: &mut Command) -> io::Result<()> {
        let procs = File::options()
            .write(true)
            .open(self.group.0.join("cgroup.procs"))?;
        // SAFETY: only write and errno are used in the pre-exec child. Zero
        // moves the calling process before user code or its first fork runs.
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

    fn exceeded(&self) -> io::Result<bool> {
        oom_killed(&fs::read_to_string(
            self.group.0.join("memory.events.local"),
        )?)
    }

    async fn wait_for_oom(&mut self) -> io::Result<()> {
        let mut events = [0u8; 256];
        loop {
            if self.exceeded()? {
                // The kernel group OOM policy handles the ordinary case. Also
                // signal remaining members of this exact subtree, including
                // descendants whose session or process group has changed.
                return self.group.kill();
            }
            if self.changes.read(&mut events).await? == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
        }
    }

    pub(crate) async fn observe<F: Future>(&mut self, outcome: F) -> F::Output {
        let mut outcome = std::pin::pin!(outcome);
        tokio::select! {
            biased;
            result = &mut outcome => result,
            watch = self.wait_for_oom() => {
                if let Err(error) = watch { eprintln!("ssmcp: observing task memory events: {error}"); }
                outcome.await
            }
        }
    }

    pub(crate) async fn finish(self) -> Receipt {
        let drained = self.group.drain().await;
        let exceeded = self.exceeded().unwrap_or_else(|error| {
            eprintln!("ssmcp: reading final task memory events: {error}");
            false
        });
        let peak = read_number(&self.group.0, "memory.peak").ok();
        let cleanup_error = drained
            .and_then(|()| fs::remove_dir(&self.group.0))
            .err()
            .map(|error| {
                let leaf = self
                    .group
                    .0
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy();
                format!("task cgroup {leaf}: {error}")
            });
        Receipt {
            exceeded,
            limit: Some(self.limit),
            peak,
            cleanup_error,
        }
    }
}

impl Group {
    fn populated(&self) -> io::Result<bool> {
        Ok(counter(
            &fs::read_to_string(self.0.join("cgroup.events"))?,
            "populated",
        )? > 0)
    }

    fn kill(&self) -> io::Result<()> {
        fs::write(self.0.join("cgroup.kill"), b"1")
    }

    async fn drain(&self) -> io::Result<()> {
        if !self.populated()? {
            return Ok(());
        }
        self.kill()?;
        let driver = Driver::get()?;
        for delay in [5, 10, 20, 40, 80, 160, 320, 400] {
            if !self.populated()? {
                return Ok(());
            }
            driver
                .timer
                .sleep(Duration::from_millis(delay))
                .await
                .map_err(io::Error::other)?;
        }
        if self.populated()? {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "still populated after cgroup.kill",
            ));
        }
        Ok(())
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        match fs::remove_dir(&self.0) {
            Ok(()) => return,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return,
            Err(_) => (),
        }
        // Failed spawn, interrupted supervision, or a process that has not
        // finished exiting. Never wait for it on the job completion path.
        if let Err(error) = self.kill() {
            eprintln!("ssmcp: cleaning task cgroup: {error}");
            return;
        }
        if fs::remove_dir(&self.0).is_ok() {
            return;
        }
        static RETRIES: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(16);
        let Ok(permit) = RETRIES.try_acquire() else {
            eprintln!(
                "ssmcp: task cgroup cleanup backlog full: {}",
                self.0.display()
            );
            return;
        };
        let path = self.0.clone();
        let schedule = Driver::get().and_then(|driver| {
            driver.spawn(async move {
                let _permit = permit;
                for seconds in [1, 2, 4, 8] {
                    if driver
                        .timer
                        .sleep(Duration::from_secs(seconds))
                        .await
                        .is_err()
                    {
                        break;
                    }
                    match fs::remove_dir(&path) {
                        Ok(()) => return,
                        Err(e) if e.kind() == io::ErrorKind::NotFound => return,
                        Err(e) if e.raw_os_error() == Some(libc::EBUSY) => (),
                        Err(e) => {
                            eprintln!("ssmcp: removing task cgroup: {e}");
                            return;
                        }
                    }
                }
                eprintln!("ssmcp: task cgroup cleanup incomplete: {}", path.display());
            })
        });
        if let Err(error) = schedule {
            eprintln!("ssmcp: scheduling task cleanup: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requires_local_oom_and_victim_and_clamps_to_both_budgets() {
        assert!(oom_killed("max 4\noom 2\noom_kill 1\n").unwrap());
        for events in [
            "oom 0\noom_kill 1",
            "oom 1\noom_kill 0",
            "oom 0\noom_kill 0",
        ] {
            assert!(!oom_killed(events).unwrap());
        }
        assert!(oom_killed("oom_kill 1").is_err());
        assert_eq!(task_budget(16 << 30, 2 << 30, 4096).unwrap(), 1 << 30);
        assert_eq!(task_budget(16 << 30, 8 << 30, 4096).unwrap(), 2 << 30);
        assert_eq!(task_budget(1 << 30, 3 * 4096, 4096).unwrap(), 4096);
    }
    #[test]
    fn requires_a_delegated_supervisor_inside_the_admitted_slice() {
        let base = Path::new(
            "/sys/fs/cgroup/ssmcp.slice/ssmcp-s0123456789abcdef0123456789abcdef.slice/unit.scope",
        );
        assert_eq!(
            delegated_unit(&base.join("ssmcp-supervisor")).unwrap().0,
            base
        );
        assert!(delegated_unit(&base.join("ssmcp-exec-abcd")).is_err());
        assert!(
            delegated_unit(Path::new(
                "/sys/fs/cgroup/app.slice/unit.scope/ssmcp-supervisor"
            ))
            .is_err()
        );
    }

    #[tokio::test]
    #[ignore = "requires an admitted Linux SSMCP session with a delegated memory controller"]
    async fn kernel_confirms_only_the_oom_leaf_and_preserves_exit_and_cancel() {
        use crate::backend::process::ExecManaged;
        use std::os::unix::process::ExitStatusExt;
        let current =
            crate::worker::limits::group_path(&fs::read_to_string("/proc/self/cgroup").unwrap())
                .unwrap();
        let admitted = CommandMemory::prepare(&current).unwrap();
        let (parent, _) = delegated_unit(&current).unwrap();
        let limit = admitted.limit.min(48 * 1024 * 1024);
        assert!(limit >= 24 * 1024 * 1024);
        drop(admitted);

        let sibling = CommandMemory::create(parent, limit).unwrap();
        let sibling_path = sibling.group.0.clone();
        let mut sleeper =
            ExecManaged::spawn_limited(Command::new("/bin/sleep").arg("15"), false, Some(&sibling))
                .unwrap();
        let mut victim = CommandMemory::create(parent, limit).unwrap();
        let victim_path = victim.group.0.clone();
        let mut command = Command::new("python3");
        command.args(["-c", "import os; pid=os.fork(); exec('os.waitpid(pid,0)' if pid else 'payload=bytearray(64*1024*1024)')"]);
        let mut child = ExecManaged::spawn_limited(&mut command, false, Some(&victim)).unwrap();
        let status = tokio::time::timeout(Duration::from_secs(10), victim.observe(child.wait()))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(status.signal(), Some(libc::SIGKILL));
        let receipt = victim.finish().await;
        assert!(receipt.exceeded && receipt.cleanup_error.is_none());
        assert_eq!(receipt.limit, Some(limit));
        assert!(receipt.peak.is_none_or(|n| n > 0));
        assert!(!victim_path.exists());
        assert!(sibling.group.populated().unwrap());

        sleeper.shutdown().await;
        assert!(!sibling.finish().await.exceeded);
        assert!(!sibling_path.exists());

        let mut regular = CommandMemory::create(parent, limit).unwrap();
        let mut child = ExecManaged::spawn_limited(
            Command::new("/bin/sh").args(["-c", "exit 7"]),
            false,
            Some(&regular),
        )
        .unwrap();
        assert_eq!(regular.observe(child.wait()).await.unwrap().code(), Some(7));
        assert!(!regular.finish().await.exceeded);
    }
}
