//! Per-session resource limits. The existing service owns restart/stop policy;
//! systemd only supplies a cgroup for each limited runner and its descendants.
#[cfg(target_os = "linux")]
pub(crate) mod scope;
use std::path::Path;

use serde::Serialize;
use tokio::process::Command;

use super::Manifest;
use crate::Result;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub(crate) struct Limits {
    cpu_percent: Option<u32>,
    memory_bytes: Option<u64>,
}

impl Limits {
    #[cfg(target_os = "linux")]
    pub(super) fn new_slice(&self) -> Result<String> {
        crate::session_budget::new_session_slice(self.cpu_percent, self.memory_bytes)
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.cpu_percent.is_none() && self.memory_bytes.is_none()
    }

    pub(crate) fn set(&mut self, flag: &str, value: &str) -> std::result::Result<(), String> {
        match flag {
            "--cpu-limit" => {
                if self.cpu_percent.is_some() {
                    return Err("--cpu-limit may be supplied only once".into());
                }
                let number = value.strip_suffix('%').unwrap_or(value);
                self.cpu_percent = Some(
                    number
                        .bytes()
                        .all(|byte| byte.is_ascii_digit())
                        .then(|| number.parse::<u32>().ok())
                        .flatten()
                        .filter(|value| *value > 0)
                        .ok_or(
                            "--cpu-limit requires a positive whole percentage, e.g. 50% or 200%",
                        )?,
                );
            }
            "--memory-limit" => {
                if self.memory_bytes.is_some() {
                    return Err("--memory-limit may be supplied only once".into());
                }
                let digits = value.bytes().take_while(u8::is_ascii_digit).count();
                let (number, suffix) = value.split_at(digits);
                let multiplier: u64 =
                    match suffix {
                        "" | "B" => 1,
                        "K" | "KiB" => 1024,
                        "M" | "MiB" => 1024 * 1024,
                        "G" | "GiB" => 1024 * 1024 * 1024,
                        "T" | "TiB" => 1024 * 1024 * 1024 * 1024,
                        _ => return Err(
                            "--memory-limit accepts bytes or K/M/G/T and KiB/MiB/GiB/TiB suffixes"
                                .into(),
                        ),
                    };
                self.memory_bytes = Some(
                    number
                        .parse::<u64>()
                        .ok()
                        .and_then(|value| value.checked_mul(multiplier))
                        .filter(|value| *value > 0)
                        .ok_or(
                            "--memory-limit must be a positive size that fits in 64-bit bytes",
                        )?,
                );
            }
            _ => return Err("unknown resource limit".into()),
        }
        Ok(())
    }

    pub(crate) fn arguments(&self) -> Vec<String> {
        let mut args = Vec::new();
        if let Some(percent) = self.cpu_percent {
            args.extend(["--cpu-limit".into(), format!("{percent}%")]);
        }
        if let Some(bytes) = self.memory_bytes {
            args.extend(["--memory-limit".into(), bytes.to_string()]);
        }
        args
    }

    pub(crate) fn require_background(&self, background: bool) -> Result<()> {
        if !self.is_empty() && !background {
            return Err(
                "--cpu-limit and --memory-limit require --daemon or sessions create".into(),
            );
        }
        self.require_backend(cfg!(target_os = "linux"))
    }

    fn require_backend(&self, linux: bool) -> Result<()> {
        if !self.is_empty() && !linux {
            return Err("resource limits require Linux systemd and cgroup v2; this backend cannot enforce them".into());
        }
        Ok(())
    }

    #[cfg(any(target_os = "linux", test))]
    fn properties(&self) -> Vec<String> {
        let mut properties = Vec::new();
        if let Some(percent) = self.cpu_percent {
            properties.push(format!("--property=CPUQuota={percent}%"));
        }
        if let Some(bytes) = self.memory_bytes {
            properties.push(format!("--property=MemoryMax={bytes}"));
        }
        properties
    }

    /// A successful manager request is not proof of controller enforcement.
    /// Verify kernel values before the saved runner can establish a connection.
    pub(super) fn verify_current(&self, unit: &str) -> Result<()> {
        if self.is_empty() {
            return Ok(());
        }
        self.require_backend(cfg!(target_os = "linux"))?;
        #[cfg(target_os = "linux")]
        return self.verify_linux(unit).map_err(|error| {
            format!("requested resource limits could not be verified: {error}; session connection was not started").into()
        });
        #[cfg(not(target_os = "linux"))]
        {
            let _ = unit;
            Err("resource limits are unavailable on this backend".into())
        }
    }

    #[cfg(target_os = "linux")]
    fn verify_linux(&self, unit: &str) -> Result<()> {
        use std::{fs::File, io::Read, mem::MaybeUninit};

        fn bounded(path: &Path, maximum: u64) -> Result<String> {
            let mut text = String::new();
            File::open(path)?
                .take(maximum + 1)
                .read_to_string(&mut text)?;
            if text.len() as u64 > maximum {
                return Err("cgroup observation exceeds its bound".into());
            }
            Ok(text)
        }

        // Do not interpret ordinary files at this path as kernel enforcement.
        let mut filesystem = MaybeUninit::<libc::statfs>::uninit();
        // SAFETY: the path is NUL-terminated and statfs initializes this buffer on success.
        let filesystem = unsafe {
            if libc::statfs(c"/sys/fs/cgroup".as_ptr(), filesystem.as_mut_ptr()) != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            filesystem.assume_init()
        };
        if filesystem.f_type as u64 != 0x6367_7270 {
            return Err("a unified cgroup v2 mount at /sys/fs/cgroup is required".into());
        }
        let membership = bounded(Path::new("/proc/self/cgroup"), 64 * 1024)?;
        let directory = cgroup_directory(&membership, unit)?;
        let cpu = self
            .cpu_percent
            .map(|_| bounded(&directory.join("cpu.max"), 128))
            .transpose()?;
        let memory = self
            .memory_bytes
            .map(|_| bounded(&directory.join("memory.max"), 128))
            .transpose()?;
        self.verify_values(cpu.as_deref(), memory.as_deref())?;
        if membership != bounded(Path::new("/proc/self/cgroup"), 64 * 1024)? {
            return Err("runner changed cgroups while resource limits were checked".into());
        }
        Ok(())
    }

    #[cfg(any(target_os = "linux", test))]
    fn verify_values(&self, cpu: Option<&str>, memory: Option<&str>) -> Result<()> {
        if let Some(percent) = self.cpu_percent {
            let mut fields = cpu.unwrap_or("").split_whitespace();
            let quota = fields
                .next()
                .and_then(|value| value.parse::<u64>().ok())
                .filter(|value| *value > 0);
            let period = fields
                .next()
                .and_then(|value| value.parse::<u64>().ok())
                .filter(|value| *value > 0);
            let enforced = quota.zip(period).is_some_and(|(quota, period)| {
                u128::from(quota) * 100 <= u128::from(period) * u128::from(percent)
            });
            if !enforced || fields.next().is_some() {
                return Err(
                    "cpu.max is missing, unlimited, malformed or exceeds --cpu-limit".into(),
                );
            }
        }
        if let Some(bytes) = self.memory_bytes
            && !memory
                .and_then(|value| value.trim().parse::<u64>().ok())
                .is_some_and(|limit| limit <= bytes)
        {
            return Err(
                "memory.max is missing, unlimited, malformed or exceeds --memory-limit".into(),
            );
        }
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn cgroup_directory(membership: &str, unit: &str) -> Result<std::path::PathBuf> {
    use std::path::Component;

    let mut unified = membership
        .lines()
        .filter_map(|line| line.strip_prefix("0::"));
    let path = unified
        .next()
        .ok_or("runner has no unified cgroup v2 membership")?;
    if unified.next().is_some() {
        return Err("runner has ambiguous unified cgroup membership".into());
    }
    let path = Path::new(path);
    let unit_path = if path
        .file_name()
        .is_some_and(|name| name == "ssmcp-supervisor")
    {
        path.parent().ok_or("missing delegated session unit")?
    } else {
        path
    };
    if !path.is_absolute()
        || path
            .components()
            .any(|part| !matches!(part, Component::RootDir | Component::Normal(_)))
        || unit_path.file_name().is_none_or(|name| name != unit)
    {
        return Err("runner is not in its expected per-session systemd unit".into());
    }
    Ok(Path::new("/sys/fs/cgroup").join(unit_path.strip_prefix("/")?))
}

pub(super) fn unit_name(manifest: &Manifest, generation: Option<&str>) -> String {
    match generation {
        Some(generation) => format!("{}-{generation}.scope", manifest.manager_id),
        None => format!("{}.service", manifest.manager_id),
    }
}

#[cfg(any(target_os = "linux", test))]
pub(crate) fn scope_options(slice: &str, unit: &str) -> Vec<String> {
    vec![
        "--user".into(),
        "--scope".into(),
        "--quiet".into(),
        "--collect".into(),
        "--property=Delegate=memory".into(),
        format!("--slice={slice}"),
        format!("--unit={unit}"),
    ]
}

#[cfg(any(target_os = "linux", test))]
fn scope_arguments(
    manifest: &Manifest,
    generation: &str,
    limits: &Limits,
    slice: &str,
) -> Vec<String> {
    let mut args = scope_options(slice, &unit_name(manifest, Some(generation)));
    args.extend(limits.properties());
    args.push("--property=MemorySwapMax=0".into());
    args
}

pub(super) fn managed_command(
    directory: &Path,
    manifest: &Manifest,
    generation: &str,
) -> Result<Command> {
    let limits = crate::client::saved_resource_limits(&manifest.spec)?;
    limits.require_background(true)?;
    let executable = super::executable(directory);
    let mut command = Command::new(&executable);
    #[cfg(target_os = "linux")]
    if !limits.is_empty() {
        // A synchronous scope retains parent/child ownership under Managed's
        // guardian. Only the existing supervisor retries failed generations.
        // Preserve the previously shipped 8 GiB ceiling for explicitly limited
        // managed runners. The session slice also enforces its physical-RAM fraction.
        let limits = Limits {
            memory_bytes: Some(
                limits
                    .memory_bytes
                    .unwrap_or(u64::MAX)
                    .min(8 * 1024 * 1024 * 1024),
            ),
            ..limits
        };
        let slice = limits.new_slice()?;
        let options = scope_arguments(manifest, generation, &limits, &slice);
        return Ok(Command::from(scope::launch(
            &options,
            &executable,
            &["__session-run".into(), manifest.spec.name.clone().into()],
        )?));
    }
    #[cfg(not(target_os = "linux"))]
    let _ = generation;
    command.args(["__session-run", &manifest.spec.name]);
    Ok(command)
}

#[cfg(any(target_os = "linux", test))]
pub(super) fn native_arguments(
    directory: &Path,
    manifest: &Manifest,
    limits: &Limits,
    slice: &str,
) -> Result<Vec<String>> {
    let mut args = vec![
        "--user".into(),
        "--quiet".into(),
        "--collect".into(),
        "--service-type=exec".into(),
        "--property=Delegate=memory".into(),
        format!("--slice={slice}"),
        format!("--unit={}", manifest.manager_id),
        format!(
            "--working-directory={}",
            super::path_text(&manifest.spec.cwd)?
        ),
        "--property=KillMode=control-group".into(),
        "--property=TimeoutStopSec=15s".into(),
        "--property=UMask=0077".into(),
        "--property=StandardInput=null".into(),
        "--property=StandardOutput=null".into(),
        "--property=StandardError=journal".into(),
        format!(
            "--property=Restart={}",
            if manifest.spec.watchdog {
                "on-failure"
            } else {
                "no"
            }
        ),
        format!(
            "--property=RestartSec={}s",
            manifest.spec.reconnect_delay_secs
        ),
        "--property=StartLimitIntervalSec=0".into(),
    ];
    args.extend(limits.properties());
    for (key, value) in &manifest.environment {
        args.push(format!("--setenv={key}={value}"));
    }
    let executable = super::executable(directory);
    let path = super::path_text(&executable)?;
    args.push("--".into());
    // Service ExecStart performs dollar expansion on every supported systemd.
    // Keep its executable field literal even when the snapshot path contains '$'.
    if path.contains('$') {
        args.extend(["/usr/bin/env".into(), "--".into()]);
    }
    args.extend([
        path.replace('$', "$$"),
        "__session-run".into(),
        manifest.spec.name.replace('$', "$$"),
    ]);
    Ok(args)
}

#[cfg(test)]
mod tests;
