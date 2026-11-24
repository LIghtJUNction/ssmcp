use super::*;
mod automatic;
mod cli_auto;
mod netns;
use crate::{
    backend, bridge,
    broker::Broker,
    device::DevicePeer,
    identity::Identity,
    wire::{self, Frame, Operation},
    workspace::Workspace,
};
use std::{
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream, UnixStream},
    task::JoinSet,
    time::timeout,
};

const UP: usize = 131_101;
const DOWN: usize = 196_619;
struct Fixture {
    broker: Arc<Broker>,
    dir: PathBuf,
    origin: String,
    token: String,
    // Independently observe payload on actual relay->client write operations.
    wire_bytes: Arc<AtomicU64>,
    workers: JoinSet<()>,
}

impl Fixture {
    async fn wait_ready(&self, peers: &[Arc<crate::broker::Session>]) {
        let auth = self.broker.authenticate("mcp", &self.token).await.unwrap();
        // Relay registration precedes the CLI's native runtime startup. Complete
        // a read-only round trip before starting the five-second offer window.
        let catalogs = futures_util::future::try_join_all(peers.iter().map(|peer| {
            peer.call_with_authorization(Operation::ListTools { cursor: None }, None, &auth)
        }))
        .await
        .expect("CLI native runtime did not become ready");
        assert!(catalogs.iter().all(|catalog| catalog["tools"].is_array()));
    }

    async fn start() -> Self {
        let mut nonce = [0; 16];
        getrandom::fill(&mut nonce).unwrap();
        let dir =
            std::env::temp_dir().join(format!("ssmcp-peer-test-{}", crate::hex::encode(nonce)));
        let http = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", http.local_addr().unwrap());
        let mut broker = Broker::new(origin.replacen("http://", "https://", 1));
        broker.public_url = origin.clone();
        broker.oauth = crate::oauth::OAuth::new(origin.clone());
        broker.identities = crate::identity::IdentityProviders::with_github_test_endpoints(
            &origin,
            crate::identity::GitHubConfig {
                client_id: "fixture".into(),
                client_secret: "fixture".into(),
            },
            &origin,
        )
        .unwrap();
        broker.portal = crate::portal::Portal::open(&dir).unwrap();
        broker
            .portal
            .storage(|store| {
                store.login(&Identity {
                    provider: "github".into(),
                    subject: "42".into(),
                    login: "alice".into(),
                })?;
                store.claim_handle("42", "alice")?;
                Ok(())
            })
            .await
            .unwrap();
        let token = broker.oauth.test_identity_tokens("mcp", "42")["access_token"]
            .as_str()
            .unwrap()
            .into();
        let broker = Arc::new(broker);
        let routes = crate::http::router(broker.clone());
        let mut workers = JoinSet::new();
        workers.spawn(async move {
            axum::serve(http, routes).await.unwrap();
        });
        Self {
            broker,
            origin,
            dir,
            token,
            wire_bytes: Arc::default(),
            workers,
        }
    }

    async fn attach(&mut self, config: Config, opted_in: bool) -> String {
        let (server, client) = UnixStream::pair().unwrap();
        let peer = DevicePeer {
            account: None,
            session_access: true,
            control_attribution: true,
            native_messages: false,
            version_notice: false,
            wait_display: None,
            time_observation: false,
            chat_disabled: false,
            openai_context: false,
            peer_ports: opted_in && config.auto_candidates.is_none(),
            peer_ports_auto: opted_in && config.auto_candidates.is_some(),
        };
        let broker = self.broker.clone();
        self.workers.spawn(async move {
            broker
                .attach_device(server, peer, Default::default())
                .await
                .unwrap();
        });
        let (r, mut w) = client.into_split();
        let mut r = BufReader::new(r);
        let metadata = wire::ClientMetadata {
            name: "peer".into(),
            title: None,
            cwd: self.dir.display().to_string(),
            hostname: "fixture".into(),
            allow: vec![format!("alice@{}", self.origin)],
        };
        wire::write_frame(
            &mut w,
            &Frame::Hello {
                version: wire::VERSION,
                mode: wire::Mode::Shell,
                metadata,
            },
        )
        .await
        .unwrap();
        let msg = wire::read_frame(&mut r).await.unwrap();
        let Some(Frame::Ready {
            client_id,
            capabilities,
            ..
        }) = msg
        else {
            panic!(
                "registration failed: {}",
                serde_json::to_string(&msg).unwrap()
            )
        };
        let capability = if config.auto_candidates.is_some() {
            wire::peer_port::AUTO_CAPABILITY
        } else {
            wire::peer_port::CAPABILITY
        };
        assert_eq!(capabilities.iter().any(|s| s == capability), opted_in);
        let peers = Arc::new(PeerPorts {
            client_id: client_id.clone(),
            policy: opted_in.then(|| config.policy().unwrap()),
            state: Mutex::default(),
        });
        let ws = Arc::new(Workspace::new(self.dir.clone()).unwrap());
        let bytes = self.wire_bytes.clone();
        self.workers.spawn(async move {
            let w = Arc::new(Mutex::new(w));
            let mut calls = JoinSet::new();
            loop {
                tokio::select! {
                    frame = wire::read_frame(&mut r) => {
                        let Ok(Some(frame)) = frame else {break};
                        let Frame::Call {id,operation,..} = frame else {continue};
                        let (ws,peers,w,bytes) = (ws.clone(),peers.clone(),w.clone(),bytes.clone());
                        calls.spawn(async move {
                            let result = match operation {
                                Operation::PeerPort { command } => { assert!(opted_in); peers.perform(*command).await }
                                Operation::CallTool {name,arguments} if name.starts_with("ssmcp.net.") => {
                                    if name == "ssmcp.net.write" {
                                        bytes.fetch_add(STANDARD.decode(arguments["data_base64"].as_str().unwrap()).unwrap().len() as u64,Ordering::SeqCst);
                                    }
                                    ws.perform(&name,arguments).await.map(backend::result)
                                }
                                _ => Err("unsupported fixture operation".into()),
                            };
                            let _ = wire::write_frame(&mut *w.lock().await,&Frame::Reply {id,result}).await;
                        });
                    }
                    Some(done) = calls.join_next(), if !calls.is_empty() => done.unwrap(),
                }
            }
            calls.shutdown().await; peers.shutdown().await; ws.shutdown().await;
        });
        client_id
    }

    async fn call(&self, name: &str, args: Value) -> Result<Value> {
        bridge::dispatch(
            self.broker.clone(),
            "mcp".into(),
            self.token.clone(),
            name.into(),
            args,
        )
        .await
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.workers.abort_all();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

async fn free(ip: &str) -> SocketAddr {
    TcpListener::bind((ip, 0))
        .await
        .unwrap()
        .local_addr()
        .unwrap()
}
fn config(role: Role, target: SocketAddr, candidate: SocketAddr) -> Config {
    Config {
        role,
        listen: "127.0.0.1:0".parse().unwrap(),
        target,
        candidate: Some(candidate),
        auto_candidates: None,
        bind: None,
        pair_key: STANDARD.encode([0x27; 32]),
    }
}

#[test]
fn policy_mode_must_match_the_negotiated_capability() {
    let target = "127.0.0.1:3000".parse().unwrap();
    let candidate = "127.0.0.2:44443".parse().unwrap();
    assert!(
        config(Role::Listen, target, candidate)
            .negotiated_policy(wire::peer_port::AUTO_CAPABILITY)
            .is_err()
    );
    let mut automatic = config(Role::Listen, target, candidate);
    automatic.candidate = None;
    automatic.auto_candidates = Some(vec!["127.0.0.0/8".into()]);
    assert!(
        automatic
            .negotiated_policy(wire::peer_port::CAPABILITY)
            .is_err()
    );
    assert!(
        config(Role::Listen, target, candidate)
            .negotiated_policy(wire::peer_port::CAPABILITY)
            .is_ok()
    );
}
fn options(left: &str, right: &str, target: SocketAddr, mode: &str) -> Value {
    json!({"listen":{"client_id":left,"port":0},"target":{"client_id":right,"port":target.port()},"transport":mode})
}

async fn roundtrip(app: &TcpListener, addr: SocketAddr) {
    let forward: Vec<u8> = (0..UP).map(|i| (i % 251) as u8).collect();
    let reverse: Vec<u8> = (0..DOWN).map(|i| (i % 239) as u8).collect();
    timeout(Duration::from_secs(12), async {
        tokio::join!(
            async {
                let (mut socket, _) = app.accept().await.unwrap();
                let mut got = Vec::new();
                socket.read_to_end(&mut got).await.unwrap();
                assert!(
                    got == forward,
                    "forward data mismatch ({} of {} bytes)",
                    got.len(),
                    UP
                );
                socket.write_all(&reverse).await.unwrap();
                socket.shutdown().await.unwrap();
            },
            async {
                let mut socket = TcpStream::connect(addr).await.unwrap();
                socket.write_all(&forward).await.unwrap();
                socket.shutdown().await.unwrap();
                let mut got = Vec::new();
                socket.read_to_end(&mut got).await.unwrap();
                assert!(
                    got == reverse,
                    "return data mismatch ({} of {} bytes)",
                    got.len(),
                    DOWN
                );
            }
        );
    })
    .await
    .unwrap();
}

async fn scenario(mode: &str, opted_in: bool, reachable: bool, expected: &str) {
    let mut f = Fixture::start().await;
    let app = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target = app.local_addr().unwrap();
    let candidate = free("127.0.0.3").await;
    let mut target_policy = config(Role::Target, target, candidate);
    if !reachable {
        target_policy.bind = Some(free("127.0.0.4").await);
    }
    let bind = target_policy.bind.unwrap_or(candidate);
    let left = f
        .attach(config(Role::Listen, target, candidate), true)
        .await;
    let right = f.attach(target_policy, opted_in).await;
    let mut args = options(&left, &right, target, mode);
    if !opted_in && mode == "prefer_direct" {
        args.as_object_mut().unwrap().remove("transport");
        let strict = options(&left, &right, target, "direct_only");
        assert!(f.call("ports.open", strict).await.is_err());
    }
    let started = f.call("ports.open", args.clone()).await;
    if expected == "failed" {
        assert!(started.unwrap_err().contains(UNREACHABLE));
        assert_eq!(
            f.call("ports.list", json!({})).await.unwrap()["ports"],
            json!([])
        );
        assert!(TcpListener::bind(bind).await.is_ok());
        return;
    }
    let opened = started.unwrap();
    assert_eq!(opened["transport"], expected, "{opened}");
    let id = &opened["id"];
    let addr = opened["listener"]["address"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    if mode == "prefer_direct" && expected == "relay" {
        assert!(opened["fallback_reason"].is_string());
    }
    assert!(
        timeout(Duration::from_millis(30), app.accept())
            .await
            .is_err(),
        "probe opened the target service"
    );
    roundtrip(&app, addr).await;
    let listed = f.call("ports.list", json!({})).await.unwrap();
    let wire_bytes = f.wire_bytes.load(Ordering::SeqCst);
    assert_eq!(
        wire_bytes,
        if expected == "direct" {
            0
        } else {
            (UP + DOWN) as u64
        }
    );
    assert_eq!(listed["ports"][0]["relay_payload_bytes"], wire_bytes);
    eprintln!(
        "mode={mode} path={expected} reachable={reachable} opted_in={opted_in} up={UP} down={DOWN} relay_payload_bytes={wire_bytes}"
    );
    let mut live = TcpStream::connect(addr).await.unwrap();
    let (mut service, _) = timeout(Duration::from_secs(4), app.accept())
        .await
        .unwrap()
        .unwrap();
    let closed = f.call("ports.close", json!({"id":id})).await.unwrap();
    assert_eq!(closed["state"], "closed");
    assert_eq!(closed["owned_handles"], 0);
    for socket in [&mut live, &mut service] {
        let mut buf = [0; 1];
        assert!(!matches!(
            timeout(Duration::from_secs(1), socket.read(&mut buf))
                .await
                .unwrap(),
            Ok(1)
        ));
    }
    assert!(TcpStream::connect(addr).await.is_err());
    assert!(TcpListener::bind(bind).await.is_ok());
}

#[tokio::test]
async fn direct_duplex_zero_relay_payload() {
    scenario("prefer_direct", true, true, "direct").await;
}
#[tokio::test]
async fn relay_duplex_payload_baseline() {
    scenario("relay", true, true, "relay").await;
}
#[tokio::test]
async fn unreachable_falls_back_once_before_application_bytes() {
    scenario("prefer_direct", true, false, "relay").await;
}
#[tokio::test]
async fn direct_only_unreachable_closes_both_sides() {
    scenario("direct_only", true, false, "failed").await;
}
#[tokio::test]
async fn legacy_peer_stays_on_relay() {
    scenario("prefer_direct", false, true, "relay").await;
}

#[tokio::test]
async fn rejects_replayed_expired_and_misdirected_grants() {
    let candidate = free("127.0.0.3").await;
    let target = free("127.0.0.1").await;
    let local = |id: &str, config: Config| {
        Arc::new(PeerPorts {
            client_id: id.into(),
            policy: Some(config.policy().unwrap()),
            state: Mutex::default(),
        })
    };
    let make = |id: &str| Mapping {
        id: id.into(),
        listen_client: "left".into(),
        target_client: "right".into(),
        listen: "127.0.0.1:0".parse().unwrap(),
        target,
        offer_until_ms: now_ms() + 3000,
        ttl_ms: 10000,
    };
    let a = local("left", config(Role::Listen, target, candidate));
    let b = local("right", config(Role::Target, target, candidate));
    let mut other = config(Role::Listen, target, candidate);
    other.pair_key = STANDARD.encode([0x82; 32]);
    let wrong = local("left", other);
    let prepare = |m: &Mapping, role| Command::Prepare {
        mapping: m.clone(),
        role,
    };
    let m = make("mapping-a");
    let mut expired = m.clone();
    expired.offer_until_ms = now_ms() - 1;
    let mut wrong_client = m.clone();
    wrong_client.listen_client = "other".into();
    let mut wrong_service = m.clone();
    wrong_service.target.set_port(0);
    for invalid in [expired, wrong_client, wrong_service] {
        assert!(a.perform(prepare(&invalid, Role::Listen)).await.is_err());
    }
    let signed: Offer =
        serde_json::from_value(a.perform(prepare(&m, Role::Listen)).await.unwrap()).unwrap();
    let wrong: Offer =
        serde_json::from_value(wrong.perform(prepare(&m, Role::Listen)).await.unwrap()).unwrap();
    let offer: Offer =
        serde_json::from_value(b.perform(prepare(&m, Role::Target)).await.unwrap()).unwrap();
    assert!(a.perform(prepare(&m, Role::Listen)).await.is_err());
    assert!(
        b.perform(Command::Activate {
            peer_available: true,
            id: offer.id.clone(),
            peer: wrong
        })
        .await
        .is_err()
    );
    assert!(
        b.perform(Command::Activate {
            peer_available: true,
            id: offer.id,
            peer: signed.clone()
        })
        .await
        .is_err()
    );
    let next = make("mapping-b");
    let offer: Offer =
        serde_json::from_value(b.perform(prepare(&next, Role::Target)).await.unwrap()).unwrap();
    assert!(
        b.perform(Command::Activate {
            peer_available: true,
            id: offer.id,
            peer: signed
        })
        .await
        .is_err()
    );
    assert!(TcpListener::bind(candidate).await.is_ok());
}

#[tokio::test]
async fn pair_mismatch_cannot_trigger_relay_fallback() {
    let mut f = Fixture::start().await;
    let target = free("127.0.0.1").await;
    let candidate = free("127.0.0.3").await;
    let left = f
        .attach(config(Role::Listen, target, candidate), true)
        .await;
    let mut other = config(Role::Target, target, candidate);
    other.pair_key = STANDARD.encode([19; 32]);
    let right = f.attach(other, true).await;
    let result = f
        .call(
            "ports.open",
            options(&left, &right, target, "prefer_direct"),
        )
        .await;
    assert!(result.unwrap_err().contains("did not authenticate"));
    assert_eq!(
        f.call("ports.list", json!({})).await.unwrap()["ports"],
        json!([])
    );
    assert!(TcpListener::bind(candidate).await.is_ok());
}

#[tokio::test]
async fn revoked_caller_loses_an_active_direct_stream() {
    let mut f = Fixture::start().await;
    let app = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target = app.local_addr().unwrap();
    let candidate = free("127.0.0.3").await;
    let left = f
        .attach(config(Role::Listen, target, candidate), true)
        .await;
    let right = f
        .attach(config(Role::Target, target, candidate), true)
        .await;
    let args = options(&left, &right, target, "direct_only");
    let other = f.broker.oauth.test_identity_tokens("mcp", "7")["access_token"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        bridge::dispatch(
            f.broker.clone(),
            "mcp".into(),
            other,
            "ports.open".into(),
            args.clone()
        )
        .await
        .is_err()
    );
    let opened = f.call("ports.open", args).await.unwrap();
    let addr = opened["listener"]["address"].as_str().unwrap();
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let (mut service, _) = timeout(Duration::from_secs(2), app.accept())
        .await
        .unwrap()
        .unwrap();
    f.broker.oauth.revoke_subject("42").unwrap();
    for socket in [&mut stream, &mut service] {
        assert!(!matches!(
            timeout(Duration::from_secs(4), socket.read(&mut [0]))
                .await
                .unwrap(),
            Ok(1)
        ));
    }
    assert!(TcpStream::connect(addr).await.is_err());
    assert!(TcpListener::bind(candidate).await.is_ok());
}

#[tokio::test]
#[ignore = "requires SSMCP_TEST_PEER_CLIENT pointing at the built ssmcp executable"]
async fn two_cli_processes_negotiate_through_the_websocket_relay() {
    use std::{fs, os::unix::fs::PermissionsExt, process::Stdio};
    let binary =
        std::fs::canonicalize(std::env::var_os("SSMCP_TEST_PEER_CLIENT").unwrap()).unwrap();
    let baseline =
        std::env::var_os("SSMCP_TEST_OLD_PEER_CLIENT").map(|p| fs::canonicalize(p).unwrap());
    let f = Fixture::start().await;
    let app = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target = app.local_addr().unwrap();
    let candidate = free("127.0.0.3").await;
    let mut programs = vec![("listen", "listen", &binary), ("target", "target", &binary)];
    if let Some(old) = &baseline {
        programs.push(("legacy", "target", old));
    }
    let mut children = Vec::new();
    for (name, role, program) in programs {
        let root = f.dir.join(name);
        let work = root.join("work");
        fs::create_dir_all(&work).unwrap();
        let policy = root.join("peer.json");
        fs::write(
            &policy,
            serde_json::to_vec(&json!({"role":role,"listen":"127.0.0.1:0",
            "target":target,"candidate":candidate,"pair_key":STANDARD.encode([0x74;32])}))
            .unwrap(),
        )
        .unwrap();
        fs::set_permissions(&policy, fs::Permissions::from_mode(0o600)).unwrap();
        let errfile = root.join("stderr");
        let child = tokio::process::Command::new(program)
            .args([
                "--server",
                &f.origin,
                "--allow",
                &format!("alice@{}", f.origin),
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
            .env(CONFIG_ENV, policy)
            .env("NO_COLOR", "1")
            .env("TERM", "dumb")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(fs::File::create(&errfile).unwrap())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        children.push((child, errfile));
    }
    let auth = f.broker.authenticate("mcp", &f.token).await.unwrap();
    let peers = timeout(Duration::from_secs(30), async {
        loop {
            let peers = f.broker.clients("mcp", &auth).await.unwrap();
            if peers.len() == children.len() {
                break peers;
            }
            for (child, log) in &mut children {
                if let Some(status) = child.try_wait().unwrap() {
                    panic!(
                        "client exited {status}: {}",
                        fs::read_to_string(log).unwrap()
                    );
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("clients to register");
    let id = |name: &str| {
        peers
            .iter()
            .find(|p| p.info().name == name)
            .unwrap()
            .client_id()
    };
    let mut modes = vec![
        ("direct_only", "target", "direct"),
        ("relay", "target", "relay"),
    ];
    if baseline.is_some() {
        modes.push(("prefer_direct", "legacy", "relay"));
    }
    for (mode, peer, expected) in modes {
        let result = f
            .call("ports.open", options(id("listen"), id(peer), target, mode))
            .await
            .unwrap();
        assert_eq!(result["transport"], expected, "{result}");
        roundtrip(
            &app,
            result["listener"]["address"]
                .as_str()
                .unwrap()
                .parse()
                .unwrap(),
        )
        .await;
        let closed = f
            .call("ports.close", json!({"id":result["id"]}))
            .await
            .unwrap();
        assert_eq!(closed["state"], "closed");
        assert_eq!(closed["owned_handles"], 0);
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
            "cli={peer} transport={expected} up={UP} down={DOWN} relay_payload_bytes={bytes}"
        );
    }
    for (mut child, _) in children {
        unsafe { libc::kill(child.id().unwrap() as i32, libc::SIGINT) };
        timeout(Duration::from_secs(10), child.wait())
            .await
            .unwrap()
            .unwrap();
    }
}

#[tokio::test]
async fn occupied_candidate_authenticates_both_policies_before_fallback() {
    let held = TcpListener::bind("127.0.0.3:0").await.unwrap();
    let candidate = held.local_addr().unwrap();
    let mut f = Fixture::start().await;
    let target = free("127.0.0.1").await;
    let left = f
        .attach(config(Role::Listen, target, candidate), true)
        .await;
    let right = f
        .attach(config(Role::Target, target, candidate), true)
        .await;
    let opened = f
        .call(
            "ports.open",
            options(&left, &right, target, "prefer_direct"),
        )
        .await
        .unwrap();
    assert_eq!(opened["transport"], "relay");
    assert!(
        timeout(Duration::from_millis(50), held.accept())
            .await
            .is_err(),
        "occupied candidate received a probe"
    );
    assert_eq!(
        f.call("ports.close", json!({"id":opened["id"]}))
            .await
            .unwrap()["state"],
        "closed"
    );
}

#[tokio::test]
async fn unauthenticated_tls_cannot_open_target_and_unrenewed_lease_expires() {
    use rustls::{
        ClientConfig, RootCertStore,
        pki_types::{CertificateDer, ServerName},
    };
    let app = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target = app.local_addr().unwrap();
    let candidate = free("127.0.0.3").await;
    let local = |id: &str, role| {
        Arc::new(PeerPorts {
            client_id: id.into(),
            policy: Some(config(role, target, candidate).policy().unwrap()),
            state: Mutex::default(),
        })
    };
    let a = local("left", Role::Listen);
    let b = local("right", Role::Target);
    let m = Mapping {
        id: "short-lease".into(),
        listen_client: "left".into(),
        target_client: "right".into(),
        listen: "127.0.0.1:0".parse().unwrap(),
        target,
        offer_until_ms: now_ms() + 3000,
        ttl_ms: 60000,
    };
    let first: Offer = serde_json::from_value(
        a.perform(Command::Prepare {
            mapping: m.clone(),
            role: Role::Listen,
        })
        .await
        .unwrap(),
    )
    .unwrap();
    let second: Offer = serde_json::from_value(
        b.perform(Command::Prepare {
            mapping: m,
            role: Role::Target,
        })
        .await
        .unwrap(),
    )
    .unwrap();
    let activate = Command::Activate {
        id: second.id.clone(),
        peer: first,
        peer_available: true,
    };
    b.perform(activate.clone()).await.unwrap();
    assert!(b.perform(activate).await.is_err());
    let mut roots = RootCertStore::empty();
    roots
        .add(CertificateDer::from(
            STANDARD.decode(second.certificate).unwrap(),
        ))
        .unwrap();
    let config =
        ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    let socket = TcpStream::connect(candidate).await.unwrap();
    let reply = timeout(Duration::from_secs(1), async {
        let mut tls = connector
            .connect(ServerName::try_from(transport::DNS).unwrap(), socket)
            .await?;
        tls.write_u8(1).await?;
        tls.read_u8().await
    })
    .await
    .unwrap();
    assert!(reply.is_err());
    assert!(
        timeout(Duration::from_millis(40), app.accept())
            .await
            .is_err()
    );
    tokio::time::sleep(LEASE + Duration::from_millis(100)).await;
    assert!(b.perform(Command::Renew { id: second.id }).await.is_err());
    assert!(TcpListener::bind(candidate).await.is_ok());
}
