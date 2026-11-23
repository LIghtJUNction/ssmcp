use super::*;

fn limits(cpu: &str, memory: &str) -> Limits {
    let mut limits = Limits::default();
    limits.set("--cpu-limit", cpu).unwrap();
    limits.set("--memory-limit", memory).unwrap();
    limits
}

fn manifest() -> Manifest {
    Manifest {
        format: super::super::FORMAT,
        supervised: true,
        spec: super::super::SessionSpec {
            name: "saved $ name".into(),
            cwd: std::env::temp_dir(),
            args: vec![
                "--cpu-limit=50%".into(),
                "--memory-limit".into(),
                "64MiB".into(),
            ],
            watchdog: true,
            reconnect_delay_secs: 7,
        },
        executable_sha256: "fixture".into(),
        version: "fixture".into(),
        manager_id: "ssmcp-session-fixture".into(),
        created_at: 0,
        environment: Default::default(),
    }
}

#[test]
fn resource_sizes_are_positive_checked_and_canonical() {
    assert_eq!(
        limits("200", "2GiB").arguments(),
        ["--cpu-limit", "200%", "--memory-limit", "2147483648"]
    );
    assert_eq!(limits("50%", "64M"), limits("50", "67108864B"));
    for bad in [
        "",
        "0",
        "0%",
        "1.5%",
        "-1",
        "+10",
        "NaN",
        "inf",
        " 20",
        "20%%",
        "4294967296",
        "10\nRestart=always",
    ] {
        assert!(Limits::default().set("--cpu-limit", bad).is_err(), "{bad}");
    }
    for bad in [
        "",
        "0",
        "0M",
        "-1",
        "+1",
        "1.5G",
        "1GB",
        "1e3",
        "1 G",
        "2MiB\n",
        "18446744073709551616",
        "18446744073709551615K",
    ] {
        assert!(
            Limits::default().set("--memory-limit", bad).is_err(),
            "{bad}"
        );
    }
    let mut limited = limits("1", "1");
    assert!(limited.set("--cpu-limit", "2").is_err());
    assert!(limited.set("--memory-limit", "2").is_err());
}

#[test]
fn resource_limits_never_silently_fall_back_to_foreground_or_unsupported_backends() {
    let limits = limits("100", "1G");
    assert!(limits.require_background(false).is_err());
    assert!(
        limits
            .require_backend(false)
            .unwrap_err()
            .to_string()
            .contains("cannot enforce")
    );
    assert!(limits.require_backend(true).is_ok());
    assert!(Limits::default().require_background(false).is_ok());
    assert!(Limits::default().require_backend(false).is_ok());
}

#[test]
fn native_systemd_arguments_apply_limits_and_keep_crash_policy() {
    let mut manifest = manifest();
    let directory = std::env::temp_dir().join("saved $ path");
    for (watchdog, restart) in [(true, "on-failure"), (false, "no")] {
        manifest.spec.watchdog = watchdog;
        let args = native_arguments(&directory, &manifest).unwrap();
        for property in [
            "--service-type=exec",
            "--property=CPUQuota=50%",
            "--property=MemoryMax=67108864",
            "--property=RestartSec=7s",
            "--property=StartLimitIntervalSec=0",
            "--property=KillMode=control-group",
            "--expand-environment=no",
        ] {
            assert!(args.iter().any(|arg| arg == property), "{property}");
        }
        assert_eq!(
            args.iter()
                .filter(|arg| arg.starts_with("--property=Restart="))
                .collect::<Vec<_>>(),
            [&format!("--property=Restart={restart}")]
        );
        assert_eq!(&args[args.len() - 2..], ["__session-run", "saved $ name"]);
        assert!(!args.iter().any(|arg| arg == "--scope"));
    }
    manifest.spec.args.clear();
    let args = native_arguments(&directory, &manifest).unwrap();
    assert!(!args.iter().any(|arg| arg.contains("CPUQuota")
        || arg.contains("MemoryMax")
        || arg == "--expand-environment=no"));
}

#[test]
fn managed_scopes_are_unique_synchronous_and_leave_retries_with_the_owner() {
    let manifest = manifest();
    let limits = limits("50", "64M");
    let first = scope_arguments(&manifest, "generation-a", &limits);
    let second = scope_arguments(&manifest, "generation-b", &limits);
    assert_ne!(first, second);
    assert!(first.contains(&"--scope".into()));
    assert!(first.contains(&"--unit=ssmcp-session-fixture-generation-a.scope".into()));
    assert!(first.contains(&"--property=CPUQuota=50%".into()));
    assert!(first.contains(&"--property=MemoryMax=67108864".into()));
    assert!(first.contains(&"--expand-environment=no".into()));
    assert!(
        !first
            .iter()
            .any(|arg| arg == "--no-block" || arg.starts_with("--property=Restart="))
    );
}

#[cfg(target_os = "linux")]
#[test]
fn manager_wraps_only_limited_runners_and_keeps_literal_argv() {
    let mut manifest = manifest();
    let directory = std::env::temp_dir().join("saved $ path");
    let command = managed_command(&directory, &manifest, "generation-a").unwrap();
    assert_eq!(
        command.as_std().get_program(),
        std::ffi::OsStr::new("systemd-run")
    );
    let args: Vec<_> = command
        .as_std()
        .get_args()
        .map(|value| value.to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        &args[args.len() - 4..],
        [
            "--",
            super::super::executable(&directory).to_str().unwrap(),
            "__session-run",
            "saved $ name"
        ]
    );
    manifest.spec.args.clear();
    let command = managed_command(&directory, &manifest, "generation-b").unwrap();
    assert_eq!(
        command.as_std().get_program(),
        super::super::executable(&directory).as_os_str()
    );
    assert_eq!(
        command
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>(),
        ["__session-run", "saved $ name"]
    );
}

#[test]
fn kernel_observations_must_prove_each_requested_limit() {
    let limits = limits("50%", "64M");
    assert!(
        limits
            .verify_values(Some("50000 100000\n"), Some("67108864\n"))
            .is_ok()
    );
    assert!(
        limits
            .verify_values(Some("10000 100000"), Some("33554432"))
            .is_ok()
    );
    for cpu in [
        None,
        Some("max 100000"),
        Some("51000 100000"),
        Some("50000 0"),
        Some("0 100000"),
        Some("50000 100000 extra"),
    ] {
        assert!(
            limits.verify_values(cpu, Some("67108864")).is_err(),
            "{cpu:?}"
        );
    }
    for memory in [
        None,
        Some("max"),
        Some("67108865"),
        Some("-1"),
        Some("67108864 extra"),
    ] {
        assert!(
            limits.verify_values(Some("50000 100000"), memory).is_err(),
            "{memory:?}"
        );
    }
    assert!(Limits::default().verify_values(None, None).is_ok());
}

#[cfg(target_os = "linux")]
#[test]
fn kernel_membership_must_be_the_expected_session_unit() {
    let unit = "ssmcp-session-fixture-generation-a.scope";
    let membership = format!("0::/user.slice/user-1000.slice/app.slice/{unit}\n");
    let directory = cgroup_directory(&membership, unit).unwrap();
    assert_eq!(
        directory,
        Path::new("/sys/fs/cgroup/user.slice/user-1000.slice/app.slice").join(unit)
    );
    for bad in [
        "2:cpu:/session.scope",
        "0::relative.scope",
        "0::/other.scope",
        "0::/../ssmcp-session-fixture-generation-a.scope",
        "0::/ssmcp-session-fixture-generation-a.scope\n0::/other.scope",
    ] {
        assert!(cgroup_directory(bad, unit).is_err(), "{bad}");
    }
}

/// Invoked only by the explicitly ignored integration test below.
#[cfg(target_os = "linux")]
#[test]
fn systemd_scope_limit_child_fixture() {
    use std::os::unix::process::CommandExt;
    let Ok(unit) = std::env::var("SSMCP_LIMIT_TEST_UNIT") else {
        return;
    };
    let directory = std::path::PathBuf::from(std::env::var_os("SSMCP_LIMIT_TEST_DIR").unwrap());
    limits("50%", "256MiB").verify_current(&unit).unwrap();
    let descendant = std::env::var_os("SSMCP_LIMIT_TEST_DESCENDANT").is_some();
    let child = if !descendant {
        // Keep the child owned and wait for it after publishing the parent PID.
        Some(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "session::limits::tests::systemd_scope_limit_child_fixture",
                    "--nocapture",
                ])
                .env("SSMCP_LIMIT_TEST_DESCENDANT", "1")
                .process_group(0)
                .spawn()
                .unwrap(),
        )
    } else {
        None
    };
    let name = if descendant {
        "descendant.pid"
    } else {
        "runner.pid"
    };
    let temporary = directory.join(format!("{name}.tmp"));
    std::fs::write(&temporary, std::process::id().to_string()).unwrap();
    // Publish readiness only after the complete PID is visible to the observer.
    std::fs::rename(temporary, directory.join(name)).unwrap();
    if let Some(mut child) = child {
        let status = child.wait().unwrap();
        panic!("owned descendant exited before guardian shutdown: {status}");
    }
    loop {
        std::thread::park();
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "requires user systemd >=254 with delegated CPU/memory cgroup v2; creates one temporary owned scope"]
async fn systemd_scope_applies_limits_and_shutdown_reaps_descendants() {
    use std::{os::fd::AsRawFd, time::Duration};
    use tokio::time::{sleep, timeout};

    struct Directory(std::path::PathBuf);
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let suffix = super::super::random_suffix().unwrap();
    let directory = Directory(std::env::temp_dir().join(format!("ssmcp-limit-test-{suffix}")));
    crate::platform::private_directory(&directory.0).unwrap();
    let mut manifest = manifest();
    manifest.manager_id = format!("ssmcp-limit-test-{suffix}");
    let unit = unit_name(&manifest, Some(&suffix));
    let limits = limits("50%", "256MiB");
    let mut command = Command::new("systemd-run");
    command
        .args(scope_arguments(&manifest, &suffix, &limits))
        .arg("--")
        .arg(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "session::limits::tests::systemd_scope_limit_child_fixture",
            "--nocapture",
        ])
        .env("SSMCP_LIMIT_TEST_UNIT", &unit)
        .env("SSMCP_LIMIT_TEST_DIR", &directory.0)
        .env_remove("SSMCP_LIMIT_TEST_DESCENDANT")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit());
    let mut process = crate::backend::process::Managed::spawn(&mut command).unwrap();
    timeout(Duration::from_secs(20), async {
        while !directory.0.join("runner.pid").is_file()
            || !directory.0.join("descendant.pid").is_file()
        {
            assert!(
                process.child.try_wait().unwrap().is_none(),
                "limited fixture exited before verifying kernel limits"
            );
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("limited fixture never became ready; inspect systemd diagnostics");
    let identities: Vec<_> = ["runner.pid", "descendant.pid"]
        .iter()
        .map(|file| {
            let pid = std::fs::read_to_string(directory.0.join(file))
                .unwrap()
                .parse::<i32>()
                .unwrap();
            crate::pty::open_pidfd(pid).unwrap()
        })
        .collect();
    timeout(Duration::from_secs(10), process.shutdown())
        .await
        .expect("owned scope did not stop");
    for identity in identities {
        let mut poll = libc::pollfd {
            fd: identity.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: this is one initialized pollfd holding an owned, exact process identity.
        assert_eq!(
            unsafe { libc::poll(&mut poll, 1, 0) },
            1,
            "a verified runner or descendant survived shutdown"
        );
        assert_ne!(poll.revents & (libc::POLLIN | libc::POLLHUP), 0);
    }
    timeout(Duration::from_secs(5), async {
        loop {
            let output = Command::new("systemctl")
                .args(["--user", "is-active", "--quiet", &unit])
                .env("LC_ALL", "C")
                .kill_on_drop(true)
                .output()
                .await
                .unwrap();
            match output.status.code() {
                Some(0) => sleep(Duration::from_millis(50)).await,
                Some(3 | 4) => break, // inactive or already collected
                other => panic!("cannot confirm test scope shutdown: {other:?}"),
            }
        }
    })
    .await
    .expect("owned test scope remained active");
}
