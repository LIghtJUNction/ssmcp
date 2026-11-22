use super::*;
use std::{fs, os::unix::fs::PermissionsExt, path::Path, process::Stdio};
use tokio::{io::copy_bidirectional, net::UnixListener};

struct Network {
    name: String,
    wrapper: PathBuf,
    cleaned: bool,
}

impl Drop for Network {
    fn drop(&mut self) {
        if self.cleaned {
            return;
        }
        // The helper checks exact ownership labels and bounds every Docker call.
        let _ = std::process::Command::new("python3")
            .arg(&self.wrapper)
            .args(["cleanup", "--network", &self.name])
            .stdout(Stdio::null())
            .status();
    }
}

impl Network {
    async fn cleanup(&mut self) {
        let output = timeout(
            Duration::from_secs(45),
            tokio::process::Command::new("python3")
                .arg(&self.wrapper)
                .args(["cleanup", "--network", &self.name])
                .kill_on_drop(true)
                .output(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            output.status.success(),
            "network cleanup: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        for network in [self.name.clone(), format!("{}-listen-home", self.name)] {
            let absent = docker_output(&["network", "inspect", &network]).await;
            assert!(
                !absent.status.success(),
                "owned Docker network remains after cleanup"
            );
            let error = String::from_utf8_lossy(&absent.stderr);
            assert!(
                error.contains("not found") || error.contains("No such network"),
                "network absence check failed: {error}"
            );
        }
        self.cleaned = true;
    }
}

async fn docker_output(args: &[&str]) -> std::process::Output {
    timeout(
        Duration::from_secs(15),
        tokio::process::Command::new("docker")
            .args(args)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .unwrap()
    .unwrap()
}

async fn docker(args: &[&str]) -> String {
    let output = docker_output(args).await;
    assert!(
        output.status.success(),
        "docker: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

async fn unix_to_tcp(listener: UnixListener, destination: SocketAddr) {
    let mut copies = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let Ok((mut local, _)) = accepted else { break };
                copies.spawn(async move {
                    if let Ok(mut remote) = TcpStream::connect(destination).await {
                        let _ = copy_bidirectional(&mut local, &mut remote).await;
                    }
                });
            }
            Some(_) = copies.join_next(), if !copies.is_empty() => {}
        }
    }
}

async fn tcp_to_unix(listener: TcpListener, destination: PathBuf) {
    let mut copies = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let Ok((mut local, _)) = accepted else { break };
                let destination = destination.clone();
                copies.spawn(async move {
                    if let Ok(mut remote) = UnixStream::connect(destination).await {
                        let _ = copy_bidirectional(&mut local, &mut remote).await;
                    }
                });
            }
            Some(_) = copies.join_next(), if !copies.is_empty() => {}
        }
    }
}

fn binary(variable: &str) -> PathBuf {
    let path = PathBuf::from(std::env::var_os(variable).expect(variable));
    assert!(path.is_absolute(), "{variable} must be an absolute path");
    fs::canonicalize(path).unwrap()
}

fn child(
    network: &Network,
    fixture: &Fixture,
    binary: &Path,
    role: &str,
    listen: SocketAddr,
    target: SocketAddr,
) -> (tokio::process::Child, PathBuf) {
    let stderr = fixture.dir.join(format!("{role}.stderr"));
    let child = tokio::process::Command::new("python3")
        .arg(&network.wrapper)
        .args(["client", "--network", &network.name, "--role", role])
        .arg("--root")
        .arg(&fixture.dir)
        .arg("--binary")
        .arg(binary)
        .args(["--origin", &fixture.origin])
        .arg("--listen")
        .arg(listen.to_string())
        .arg("--target")
        .arg(target.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(fs::File::create(&stderr).unwrap())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    (child, stderr)
}

#[tokio::test]
#[ignore = "requires Docker with cached alpine:latest, SSMCP_TEST_PEER_NETNS_WRAPPER and SSMCP_TEST_PEER_CLIENT"]
async fn actual_cli_discovers_host_candidates_across_two_private_network_namespaces() {
    let executable = binary("SSMCP_TEST_PEER_CLIENT");
    let wrapper = binary("SSMCP_TEST_PEER_NETNS_WRAPPER");
    let mut fixture = Fixture::start().await;
    let mut network = Network {
        name: fixture.dir.file_name().unwrap().to_str().unwrap().into(),
        wrapper,
        cleaned: false,
    };
    for name in [
        network.name.clone(),
        format!("{}-listen-home", network.name),
    ] {
        docker(&[
            "network",
            "create",
            "--internal",
            "--label",
            &format!("ssmcp.peer-netns={}", network.name),
            &name,
        ])
        .await;
    }
    let details: Value =
        serde_json::from_str(&docker(&["network", "inspect", &network.name]).await).unwrap();
    assert_eq!(details[0]["Internal"], true);
    let subnet = details[0]["IPAM"]["Config"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|entry| entry["Subnet"].as_str())
        .find(|subnet| {
            subnet
                .split('/')
                .next()
                .unwrap()
                .parse::<std::net::Ipv4Addr>()
                .is_ok()
        })
        .expect("Docker must provide an IPv4 subnet");
    let app = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target = app.local_addr().unwrap();
    // Reserve the host test entry; the client binds the same port in its own namespace.
    let application = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let listen = application.local_addr().unwrap();
    let control = UnixListener::bind(fixture.dir.join("control.sock")).unwrap();
    let service = UnixListener::bind(fixture.dir.join("target.sock")).unwrap();
    fixture.workers.spawn(unix_to_tcp(
        control,
        fixture
            .origin
            .strip_prefix("http://")
            .unwrap()
            .parse()
            .unwrap(),
    ));
    fixture.workers.spawn(unix_to_tcp(service, target));
    fixture
        .workers
        .spawn(tcp_to_unix(application, fixture.dir.join("listen.sock")));
    for role in ["listen", "target"] {
        let home = fixture.dir.join(role);
        fs::create_dir_all(home.join("work")).unwrap();
        let path = home.join("peer.json");
        fs::write(
            &path,
            serde_json::to_vec(&json!({
                "role":role, "listen":listen, "target":target,
                "auto_candidates":[subnet], "pair_key":STANDARD.encode([0x52;32])
            }))
            .unwrap(),
        )
        .unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    let mut children: Vec<_> = ["listen", "target"]
        .into_iter()
        .map(|role| child(&network, &fixture, &executable, role, listen, target))
        .collect();
    let auth = fixture
        .broker
        .authenticate("mcp", &fixture.token)
        .await
        .unwrap();
    let peers = timeout(Duration::from_secs(40), async {
        loop {
            let peers = fixture.broker.clients("mcp", &auth).await.unwrap();
            if peers.len() == 2 {
                break peers;
            }
            for (child, log) in &mut children {
                if let Some(status) = child.try_wait().unwrap() {
                    panic!(
                        "netns client exited {status}: {}",
                        fs::read_to_string(log).unwrap()
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
        panic!("netns client registration timed out: {logs:?}");
    });
    fixture.wait_ready(&peers).await;
    // Some Docker-compatible runtimes cannot remove a container's first
    // network. Its private primary network has no route to the target; the
    // shared candidate network is a detachable secondary interface.
    docker(&[
        "network",
        "connect",
        &network.name,
        &format!("{}-listen", network.name),
    ])
    .await;
    assert!(
        peers
            .iter()
            .all(|peer| peer.peer_ports_auto && !peer.peer_ports)
    );
    let mut namespaces = Vec::new();
    for role in ["listen", "target"] {
        let container = format!("{}-{role}", network.name);
        let namespace =
            docker(&["exec", &container, "/usr/bin/readlink", "/proc/self/ns/net"]).await;
        assert!(
            namespace.trim().starts_with("net:["),
            "missing network namespace for {role}"
        );
        namespaces.push(namespace);
    }
    assert_ne!(
        namespaces[0], namespaces[1],
        "clients share a network namespace"
    );
    let id = |role: &str| {
        peers
            .iter()
            .find(|peer| peer.info().name == role)
            .unwrap()
            .client_id()
    };
    for (requested, expected, reachable) in [
        ("direct_only", "direct", true),
        ("relay", "relay", true),
        ("prefer_direct", "relay", false),
    ] {
        if !reachable {
            // The private Unix control channel stays alive while the listener
            // loses its route to the target. No host firewall is modified.
            docker(&[
                "network",
                "disconnect",
                &network.name,
                &format!("{}-listen", network.name),
            ])
            .await;
        }
        let mut input = options(id("listen"), id("target"), target, requested);
        input["listen"]["port"] = json!(listen.port());
        let opened = fixture.call("ports.open", input).await.unwrap();
        assert_eq!(opened["state"], "running", "{opened}");
        assert_eq!(opened["transport"], expected, "{opened}");
        if !reachable {
            assert_eq!(opened["fallback_reason"], UNREACHABLE);
        }
        assert_eq!(opened["listener"]["port"], listen.port(), "{opened}");
        roundtrip(&app, listen).await;
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
            "private_netns=2 network={} discovery=actual_interfaces subnet={subnet} reachable={reachable} transport={expected} up={UP} down={DOWN} relay_payload_bytes={bytes}",
            network.name
        );
    }
    let mut strict = options(id("listen"), id("target"), target, "direct_only");
    strict["listen"]["port"] = json!(listen.port());
    assert!(
        fixture
            .call("ports.open", strict)
            .await
            .unwrap_err()
            .contains(UNREACHABLE)
    );
    assert_eq!(
        fixture.call("ports.list", json!({})).await.unwrap()["ports"],
        json!([])
    );
    for (mut child, log) in children.drain(..) {
        if let Some(pid) = child.id() {
            unsafe { libc::kill(pid as i32, libc::SIGINT) };
        }
        let status = timeout(Duration::from_secs(15), child.wait())
            .await
            .unwrap()
            .unwrap();
        assert!(
            status.success(),
            "netns shutdown {status}: {}",
            fs::read_to_string(log).unwrap()
        );
    }
    network.cleanup().await;
}
