//! Per-user aggregate and per-session CPU, memory and swap budgets on Linux.
//! The manager owns all units. We never stop a slice or write into another unit's cgroup.
mod kernel;

use crate::Result;
use kernel::{Budget, check_move, membership};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant},
};

const PARENT: &str = "ssmcp.slice";

pub(crate) fn manager_command(program: &str) -> Result<Command> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    if !matches!(program, "systemctl" | "systemd-run" | "busctl") {
        return Err("unsupported systemd manager tool".into());
    }
    let uid = unsafe { libc::geteuid() };
    let runtime = PathBuf::from(format!("/run/user/{uid}"));
    let dir = fs::symlink_metadata(&runtime)
        .map_err(|e| format!("session budget requires the current UID's systemd runtime: {e}"))?;
    let socket = runtime.join("bus");
    let bus = fs::symlink_metadata(&socket)
        .map_err(|e| format!("session budget requires the current UID's systemd user bus: {e}"))?;
    if !dir.is_dir()
        || dir.uid() != uid
        || dir.mode() & 0o077 != 0
        || !bus.file_type().is_socket()
        || bus.uid() != uid
    {
        return Err("unverified systemd runtime directory or user bus owner".into());
    }
    let executable = ["/usr/bin", "/bin"]
        .map(|bin| Path::new(bin).join(program))
        .into_iter()
        .find(|p| p.is_file())
        .ok_or("systemd tool unavailable on this host")?;
    let mut cmd = Command::new(executable);
    cmd.env("XDG_RUNTIME_DIR", &runtime)
        .env(
            "DBUS_SESSION_BUS_ADDRESS",
            format!("unix:path={}", socket.display()),
        )
        .env_remove("SYSTEMD_BUS_ADDRESS")
        .env_remove("SYSTEMD_BUS_TRANSPORT")
        .env_remove("SYSTEMD_HOST")
        .env_remove("SYSTEMD_MACHINE");
    Ok(cmd)
}

const LEADER: &str = "ssmcp-supervisor";

/// Prepare only the systemd-delegated unit of the calling session process.
/// Other, more deeply nested processes keep their current constraints.
fn prepare_leaf(slice: &Path, group: &Path) -> Result<()> {
    let relative = group.strip_prefix(slice)?;
    let unit_name = relative
        .iter()
        .next()
        .and_then(|s| s.to_str())
        .ok_or("missing session unit")?;
    if !(unit_name.ends_with(".scope") || unit_name.ends_with(".service")) {
        return Err("invalid unit below the session slice".into());
    }
    let unit = slice.join(unit_name);
    let supervisor = unit.join(LEADER);
    if group != unit && group != supervisor {
        return Ok(());
    }
    let enabled = || -> Result<bool> {
        Ok(fs::read_to_string(unit.join("cgroup.subtree_control"))?
            .split_whitespace()
            .any(|s| s == "memory"))
    };
    if group == supervisor && enabled()? {
        return Ok(());
    }
    let delegation = manager_command("systemctl")?
        .args([
            "--user", "show", "--value", "-p", "Delegate", "--", unit_name,
        ])
        .output()?;
    if !delegation.status.success() || delegation.stdout != b"yes\n" {
        return Err(format!("unit {unit_name} needs Delegate=memory").into());
    }
    if membership()? != group {
        return Err("unit membership changed before delegation".into());
    }
    if group == unit {
        match fs::create_dir(&supervisor) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.into()),
        }
        fs::write(
            supervisor.join("cgroup.procs"),
            std::process::id().to_string(),
        )?;
    }
    if membership()? != supervisor {
        return Err("failed to enter the session supervisor subgroup".into());
    }
    if !enabled()? {
        fs::write(unit.join("cgroup.subtree_control"), "+memory")
            .map_err(|e| format!("enable delegated memory controller for {unit_name}: {e}"))?;
    }
    if !enabled()? || !supervisor.join("memory.max").is_file() {
        return Err("kernel did not enable memory control for job subgroups".into());
    }
    Ok(())
}

struct Layout {
    parent: PathBuf,
    aggregate: Budget,
    session: Budget,
}

impl Layout {
    fn discover() -> Result<Self> {
        kernel::check_mount()?;
        let output = manager_command("systemctl")?
            .args([
                "--user",
                "--no-pager",
                "show",
                "--value",
                "--property=ControlGroup",
                "--",
                "-.slice",
            ])
            .output()?;
        if !output.status.success() {
            return Err(format!(
                "user systemd manager unavailable: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )
            .into());
        }
        let group = std::str::from_utf8(&output.stdout)?.trim();
        let root = crate::worker::limits::group_path(&format!("0::{group}"))?;
        // A user manager owns one delegated hierarchy for this UID. Do not accept a
        // container/system manager root as a different shared budget for the same UID.
        let uid = unsafe { libc::geteuid() };
        if root.file_name().and_then(|s| s.to_str()) != Some(format!("user@{uid}.service").as_str())
        {
            return Err("could not identify this UID's systemd user manager cgroup".into());
        }
        let capacity = Budget::capacity(&root)?;
        Ok(Self {
            parent: root.join(PARENT),
            aggregate: capacity.fraction(3, 4, 2, None, None)?,
            session: capacity.fraction(1, 2, 4, None, None)?,
        })
    }

    fn ensure_parent(&self) -> Result<()> {
        if self.parent.is_dir() && self.aggregate.verify(&self.parent).is_ok() {
            return Ok(());
        }
        // mode=fail will not replace an existing unit. A concurrent creator can win;
        // the only accepted outcome is the kernel readback of this exact parent.
        let props = budget_properties(self.aggregate);
        let started = start_unit(PARENT, &props, &[]);
        wait_until(|| {
            self.aggregate.verify(&self.parent)?;
            Ok(())
        })
        .map_err(|error| {
            format!(
                "cannot enforce the shared {PARENT} ceiling: {error}; manager result: {started:?}"
            )
            .into()
        })
    }

    fn inherited(&self, group: &Path) -> Result<Option<String>> {
        let Ok(rest) = group.strip_prefix(&self.parent) else {
            return Ok(None);
        };
        let name = rest
            .iter()
            .next()
            .and_then(|s| s.to_str())
            .ok_or("missing session slice")?;
        let serial = name
            .strip_prefix("ssmcp-s")
            .and_then(|s| s.strip_suffix(".slice"))
            .ok_or("unexpected unit inside the shared SSMCP slice")?;
        if serial.len() != 32
            || !serial.bytes().all(|c| c.is_ascii_hexdigit())
            || rest.iter().count() < 2
        {
            return Err("invalid SSMCP session membership".into());
        }
        self.aggregate.verify(&self.parent)?;
        self.session.verify(&self.parent.join(name))?;
        if membership()? != group {
            return Err("session changed cgroups during verification".into());
        }
        Ok(Some(name.to_owned()))
    }
}

struct Property(&'static str, &'static str, Vec<String>);

fn scalar(name: &'static str, ty: &'static str, value: impl ToString) -> Property {
    Property(name, ty, vec![value.to_string()])
}

fn budget_properties(budget: Budget) -> Vec<Property> {
    vec![
        scalar("CPUQuotaPerSecUSec", "t", budget.cpu),
        scalar("CPUQuotaPeriodUSec", "t", 1_000_000),
        scalar("MemoryMax", "t", budget.memory),
        scalar("MemorySwapMax", "t", 0),
    ]
}

fn append_properties(args: &mut Vec<String>, props: &[Property]) {
    args.push(props.len().to_string());
    for Property(name, ty, values) in props {
        args.extend([name.to_string(), ty.to_string()]);
        args.extend(values.iter().cloned());
    }
}

fn slice_properties(budget: Budget) -> Vec<Property> {
    let mut props = budget_properties(budget);
    props.push(scalar("StopWhenUnneeded", "b", true));
    props.push(scalar("CollectMode", "s", "inactive-or-failed"));
    props
}

fn start_unit(name: &str, props: &[Property], auxiliary: &[(&str, &[Property])]) -> Result<()> {
    let mut args = vec![name.to_owned(), "fail".into()];
    append_properties(&mut args, props);
    args.push(auxiliary.len().to_string());
    for (unit, properties) in auxiliary {
        args.push((*unit).to_owned());
        append_properties(&mut args, properties);
    }
    let output = manager_command("busctl")?
        .args([
            "--user",
            "--timeout=5",
            "call",
            "org.freedesktop.systemd1",
            "/org/freedesktop/systemd1",
            "org.freedesktop.systemd1.Manager",
            "StartTransientUnit",
            "ssa(sv)a(sa(sv))",
        ])
        .args(args)
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "starting {name}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }
    Ok(())
}

fn wait_until<T>(check: impl Fn() -> Result<T>) -> Result<T> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match check() {
            Ok(value) => return Ok(value),
            Err(error) if Instant::now() >= deadline => return Err(error),
            Err(_) => thread::sleep(Duration::from_millis(20)),
        }
    }
}

fn identity() -> Result<String> {
    let uuid = fs::read_to_string("/proc/sys/kernel/random/uuid")?
        .trim()
        .replace('-', "");
    if uuid.len() != 32 || !uuid.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("invalid session UUID".into());
    }
    Ok(uuid)
}

/// Admit the current process before it starts command or plugin children. A child
/// already in a valid slice stays in its exact leaf, including any tighter limits.
/// Linux library hosts should call this before starting threads that spawn children:
/// admission migrates the calling process, and existing children cannot be moved with it.
/// Repeated callers serialize admission and verify the current kernel placement.
pub fn enter() -> Result<()> {
    static ADMISSION: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = ADMISSION
        .lock()
        .map_err(|_| "session admission lock poisoned")?;
    let layout = Layout::discover()?;
    let from = membership()?;
    if let Some(slice) = layout.inherited(&from)? {
        return prepare_leaf(&layout.parent.join(slice), &from);
    }
    check_move(&from, &layout.parent)?;
    layout.ensure_parent()?;
    let id = identity()?;
    let scope = format!("ssmcp-client-{id}.scope");
    let slice = format!("ssmcp-s{id}.slice");
    let props = [
        scalar("Slice", "s", &slice),
        Property(
            "DelegateControllers",
            "as",
            vec!["1".into(), "memory".into()],
        ),
        Property(
            "PIDs",
            "au",
            vec!["1".into(), std::process::id().to_string()],
        ),
        scalar("CollectMode", "s", "inactive-or-failed"),
    ];
    start_unit(
        &scope,
        &props,
        &[(&slice, &slice_properties(layout.session))],
    )?;
    let expected = layout.parent.join(&slice).join(&scope);
    wait_until(|| {
        let current = membership()?;
        if current != expected && current != expected.join(LEADER) {
            return Err("systemd has not placed this PID in its reserved session scope".into());
        }
        layout
            .inherited(&current)?
            .ok_or("missing verified session ancestors")?;
        Ok(current)
    })
    .and_then(|current| prepare_leaf(&layout.parent.join(slice), &current))
}

/// Reserve a session slice for a subsequent systemd-run service or scope. The
/// bounded manager timer holds its dependency while the caller starts
/// the real unit. Even a crashed caller leaves no permanent per-session setting.
pub(crate) fn new_session_slice(
    cpu_percent: Option<u32>,
    memory_bytes: Option<u64>,
) -> Result<String> {
    let layout = Layout::discover()?;
    check_move(&membership()?, &layout.parent)?;
    layout.ensure_parent()?;
    let budget = layout
        .session
        .fraction(1, 1, 1, cpu_percent, memory_bytes)?;
    let id = identity()?;
    let slice = format!("ssmcp-s{id}.slice");
    // A manager timer references the slice during handoff without charging a
    // sleeping process to a potentially very small custom session budget. After
    // one expiration the timer and its no-op trigger are garbage collected.
    let timer = format!("ssmcp-admit-{id}.timer");
    let trigger = format!("ssmcp-admit-{id}.service");
    let reservation = [
        Property("Requires", "as", vec!["1".into(), slice.clone()]),
        Property(
            "TimersMonotonic",
            "a(st)",
            ["1", "OnActiveSec", "30000000"].map(String::from).to_vec(),
        ),
        scalar("AccuracyUSec", "t", 100_000),
        scalar("RemainAfterElapse", "b", false),
        scalar("CollectMode", "s", "inactive-or-failed"),
    ];
    let noop = [
        scalar("Type", "s", "oneshot"),
        Property(
            "ExecStart",
            "a(sasb)",
            ["1", "/usr/bin/true", "1", "true", "false"]
                .map(String::from)
                .to_vec(),
        ),
        scalar("CollectMode", "s", "inactive-or-failed"),
    ];
    start_unit(
        &timer,
        &reservation,
        &[(&slice, &slice_properties(budget)), (&trigger, &noop)],
    )?;
    wait_until(|| {
        budget.verify(&layout.parent.join(&slice))?;
        Ok(())
    })?;
    Ok(slice)
}

/// Destination for a worker systemd-run scope. Siblings stay within the aggregate
/// and custom session quotas; an additional restriction in the old leaf must survive.
pub(crate) fn current_session_slice() -> Result<String> {
    let layout = Layout::discover()?;
    let group = membership()?;
    let slice = layout
        .inherited(&group)?
        .ok_or("worker parent has no verified session budget")?;
    // A task subprocess may already be inside its own delegated limit. It
    // cannot request a sibling scope outside that task, even if the CPU and
    // memory ceilings happen to be numerically identical.
    let session = layout.parent.join(&slice);
    let relative = group.strip_prefix(&session)?;
    if relative.iter().count() != 2 || relative.file_name().is_none_or(|name| name != LEADER) {
        return Err("only the session supervisor may launch a sibling scope".into());
    }
    check_move(&group, &session)?;
    Ok(slice)
}
