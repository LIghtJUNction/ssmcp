use super::*;
use std::{fs, os::unix::fs::PermissionsExt, path::Path, process::Stdio};

fn program(variable: &str, required: bool) -> Option<PathBuf> {
    let Some(path) = std::env::var_os(variable).map(PathBuf::from) else {
        assert!(!required, "{variable} must name the built ssmcp executable");
        eprintln!("cli compatibility skipped: {variable} is unset");
        return None;
    };
    assert!(path.is_absolute(), "{variable} must be an absolute path");
    Some(fs::canonicalize(path).unwrap())
}

fn launch(
    fixture: &Fixture,
    executable: &Path,
    name: &str,
    role: &str,
    automatic: bool,
    target: SocketAddr,
    candidate: SocketAddr,
) -> (tokio::process::Child, PathBuf) {
    let root = fixture.dir.join(name);
    let work = root.join("work");
    fs::create_dir_all(&work).unwrap();
    let mut policy = json!({"role":role,"listen":"127.0.0.1:0",
        "target":target,"pair_key":STANDARD.encode([0x74;32])});
    if automatic {
        policy["auto_candidates"] = json!(["127.0.0.0/8"]);
    } else {
        policy["candidate"] = json!(candidate);
    }
    let policy_path = root.join("peer.json");
    fs::write(&policy_path, serde_json::to_vec(&policy).unwrap()).unwrap();
    fs::set_permissions(&policy_path, fs::Permissions::from_mode(0o600)).unwrap();
    let stderr = root.join("stderr");
    let child = tokio::process::Command::new(executable)
        .args([
            "--server",
            &fixture.origin,
            "--allow",
            &format!("alice@{}", fixture.origin),
            "--name",
            name,
            "--no-setup",
            "--input",
            "none",
            "--shell",
            "/bin/sh",
        ])
        .current_dir(&work)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", &root)
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("SSMCP_AUTH_HOME", root.join("auth"))
        .env("SSMCP_SESSION_HOME", root.join("sessions"))
        .env(CONFIG_ENV, policy_path)
        .env("NO_COLOR", "1")
        .env("TERM", "dumb")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(fs::File::create(&stderr).unwrap())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    eprintln!(
        "cli started name={name} role={role} automatic={automatic} executable={}",
        executable.display()
    );
    (child, stderr)
}

#[tokio::test]
#[ignore = "requires SSMCP_TEST_PEER_CLIENT; optional SSMCP_TEST_PR179_CLIENT and SSMCP_TEST_OLD_PEER_CLIENT"]
async fn real_cli_automatic_candidates_and_static_compatibility() {
    let current = program("SSMCP_TEST_PEER_CLIENT", true).unwrap();
    let pr179 = program("SSMCP_TEST_PR179_CLIENT", false);
    let baseline = program("SSMCP_TEST_OLD_PEER_CLIENT", false);
    let fixture = Fixture::start().await;
    let app = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target = app.local_addr().unwrap();
    let candidate = free("127.0.0.3").await;
    let mut programs = vec![
        ("auto-listen", "listen", true, &current),
        ("auto-target", "target", true, &current),
        ("static-listen", "listen", false, &current),
        ("static-target", "target", false, &current),
    ];
    if let Some(original) = &pr179 {
        programs.push(("pr179-target", "target", false, original));
    }
    if let Some(old) = &baseline {
        programs.push(("legacy-target", "target", false, old));
    }
    let mut children: Vec<_> = programs
        .iter()
        .map(|(name, role, automatic, executable)| {
            launch(
                &fixture, executable, name, role, *automatic, target, candidate,
            )
        })
        .collect();
    let auth = fixture
        .broker
        .authenticate("mcp", &fixture.token)
        .await
        .unwrap();
    let peers = timeout(Duration::from_secs(30), async {
        loop {
            let peers = fixture.broker.clients("mcp", &auth).await.unwrap();
            if peers.len() == children.len() {
                break peers;
            }
            for (child, stderr) in &mut children {
                if let Some(status) = child.try_wait().unwrap() {
                    panic!(
                        "client exited {status}: {}",
                        fs::read_to_string(stderr).unwrap()
                    );
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        let logs: Vec<_> = children
            .iter()
            .map(|(_, path)| fs::read_to_string(path).unwrap())
            .collect();
        panic!("CLI registration timed out: {logs:?}");
    });
    fixture.wait_ready(&peers).await;
    let session = |name: &str| peers.iter().find(|p| p.info().name == name).unwrap();
    for (name, _, automatic, _) in &programs {
        let peer = session(name);
        assert_eq!(peer.peer_ports_auto, *automatic, "{name}");
        assert_eq!(
            peer.peer_ports,
            !automatic && *name != "legacy-target",
            "{name}"
        );
    }
    let mut scenarios = vec![
        ("auto-listen", "auto-target", "direct_only", "direct"),
        ("auto-listen", "auto-target", "prefer_direct", "direct"),
        ("auto-listen", "auto-target", "relay", "relay"),
        ("auto-listen", "static-target", "prefer_direct", "relay"),
        ("static-listen", "auto-target", "prefer_direct", "relay"),
        ("static-listen", "static-target", "direct_only", "direct"),
    ];
    if pr179.is_some() {
        scenarios.extend([
            ("auto-listen", "pr179-target", "prefer_direct", "relay"),
            ("static-listen", "pr179-target", "direct_only", "direct"),
        ]);
    }
    if baseline.is_some() {
        scenarios.extend([
            ("auto-listen", "legacy-target", "prefer_direct", "relay"),
            ("static-listen", "legacy-target", "prefer_direct", "relay"),
        ]);
    }
    for (left, right, requested, expected) in scenarios {
        let opened = fixture
            .call(
                "ports.open",
                options(
                    session(left).client_id(),
                    session(right).client_id(),
                    target,
                    requested,
                ),
            )
            .await
            .unwrap_or_else(|error| {
                let logs: Vec<_> = children
                    .iter()
                    .map(|(_, path)| fs::read_to_string(path).unwrap())
                    .collect();
                panic!("{left} -> {right} ({requested}): {error}; client logs: {logs:?}");
            });
        assert_eq!(opened["state"], "running", "{opened}");
        assert_eq!(opened["transport"], expected, "{opened}");
        if expected == "relay" && requested != "relay" {
            assert!(opened["fallback_reason"].as_str().is_some(), "{opened}");
        }
        roundtrip(
            &app,
            opened["listener"]["address"]
                .as_str()
                .unwrap()
                .parse()
                .unwrap(),
        )
        .await;
        let closed = fixture
            .call("ports.close", json!({"id":opened["id"]}))
            .await
            .unwrap();
        assert_eq!(closed["state"], "closed", "{closed}");
        assert_eq!(closed["owned_handles"], 0, "{closed}");
        let bytes = closed["relay_payload_bytes"].as_u64().unwrap();
        assert_eq!(
            bytes,
            if expected == "direct" {
                0
            } else {
                (UP + DOWN) as u64
            }
        );
        eprintln!(
            "cli left={left} right={right} requested={requested} transport={expected} up={UP} down={DOWN} relay_payload_bytes={bytes}"
        );
    }
    for (mut child, stderr) in children {
        if let Some(id) = child.id() {
            unsafe { libc::kill(id as i32, libc::SIGINT) };
        }
        let status = timeout(Duration::from_secs(10), child.wait())
            .await
            .unwrap()
            .unwrap();
        assert!(
            status.success(),
            "client shutdown {status}: {}",
            fs::read_to_string(stderr).unwrap()
        );
    }
}
