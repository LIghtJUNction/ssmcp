use super::*;

fn automatic(role: Role, target: SocketAddr) -> Config {
    Config {
        role,
        listen: "127.0.0.1:0".parse().unwrap(),
        target,
        candidate: None,
        auto_candidates: Some(vec!["127.0.0.0/8".into()]),
        bind: None,
        pair_key: STANDARD.encode([0x27; 32]),
    }
}

fn local(id: &str, config: Config) -> Arc<PeerPorts> {
    Arc::new(PeerPorts {
        client_id: id.into(),
        policy: Some(config.policy().unwrap()),
        state: Mutex::default(),
    })
}

fn mapping(id: &str, target: SocketAddr) -> Mapping {
    Mapping {
        id: id.into(),
        listen_client: "left".into(),
        target_client: "right".into(),
        listen: "127.0.0.1:0".parse().unwrap(),
        target,
        offer_until_ms: now_ms() + 3000,
        ttl_ms: 60_000,
    }
}

async fn prepare(peers: &Arc<PeerPorts>, mapping: &Mapping, role: Role) -> Offer {
    serde_json::from_value(
        peers
            .perform(Command::Prepare {
                mapping: mapping.clone(),
                role,
            })
            .await
            .unwrap(),
    )
    .unwrap()
}

fn resign(peers: &PeerPorts, mapping: &Mapping, role: Role, offer: &mut Offer) {
    let policy = peers.policy.as_ref().unwrap();
    offer.proof = STANDARD.encode(hmac::sign(
        &policy.key,
        &transcript(mapping, policy, role, offer).unwrap(),
    ));
}

async fn assert_no_connection(listener: &TcpListener) {
    assert!(
        timeout(Duration::from_millis(50), listener.accept())
            .await
            .is_err(),
        "rejected candidate received a connection"
    );
}

async fn close(fixture: &Fixture, opened: &Value, bytes: u64) {
    let closed = fixture
        .call("ports.close", json!({"id":opened["id"]}))
        .await
        .unwrap();
    assert_eq!(closed["state"], "closed", "{closed}");
    assert_eq!(closed["owned_handles"], 0);
    assert_eq!(closed["relay_payload_bytes"], bytes);
}

#[tokio::test]
async fn discovered_hosts_forward_duplex_without_a_configured_peer_address() {
    let mut fixture = Fixture::start().await;
    let app = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target = app.local_addr().unwrap();
    let left = fixture.attach(automatic(Role::Listen, target), true).await;
    let right = fixture.attach(automatic(Role::Target, target), true).await;
    let opened = fixture
        .call("ports.open", options(&left, &right, target, "direct_only"))
        .await
        .unwrap();
    assert_eq!(opened["transport"], "direct", "{opened}");
    assert_no_connection(&app).await;
    let address: SocketAddr = opened["listener"]["address"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    roundtrip(&app, address).await;
    assert_eq!(fixture.wire_bytes.load(Ordering::SeqCst), 0);
    let mut stream = TcpStream::connect(address).await.unwrap();
    let (mut service, _) = timeout(Duration::from_secs(2), app.accept())
        .await
        .unwrap()
        .unwrap();
    close(&fixture, &opened, 0).await;
    for stream in [&mut stream, &mut service] {
        assert!(!matches!(
            timeout(Duration::from_secs(1), stream.read(&mut [0]))
                .await
                .unwrap(),
            Ok(1)
        ));
    }
    assert!(TcpStream::connect(address).await.is_err());
    eprintln!("automatic host discovery: direct up={UP} down={DOWN} relay_payload_bytes=0");
}

#[tokio::test]
async fn failed_candidate_selects_a_later_authenticated_host_and_shutdown_closes_all_listeners() {
    let app = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target = app.local_addr().unwrap();
    let dual_stack = |role| {
        let mut config = automatic(role, target);
        config.auto_candidates = Some(vec!["127.0.0.1/32".into(), "::1/128".into()]);
        config
    };
    let left = local("left", dual_stack(Role::Listen));
    let right = local("right", dual_stack(Role::Target));
    let mapping = mapping("failed-first-candidate", target);
    let left_offer = prepare(&left, &mapping, Role::Listen).await;
    let mut right_offer = prepare(&right, &mapping, Role::Target).await;
    let reachable = right_offer.candidates.clone().unwrap();
    assert!(!reachable.is_empty());
    // Keep this port reserved without listen() throughout the experiment. A
    // different test cannot accidentally occupy it and change the failure mode.
    let refused = tokio::net::TcpSocket::new_v4().unwrap();
    refused.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    right_offer
        .candidates
        .as_mut()
        .unwrap()
        .insert(0, refused.local_addr().unwrap());
    resign(&right, &mapping, Role::Target, &mut right_offer);
    right
        .perform(Command::Activate {
            id: right_offer.id.clone(),
            peer: left_offer.clone(),
            peer_available: true,
        })
        .await
        .unwrap();
    let opened = left
        .perform(Command::Activate {
            id: left_offer.id,
            peer: right_offer,
            peer_available: true,
        })
        .await
        .unwrap();
    assert_no_connection(&app).await;
    let address: SocketAddr = opened["listener"]["address"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    roundtrip(&app, address).await;
    let mut stream = TcpStream::connect(address).await.unwrap();
    let (mut service, _) = timeout(Duration::from_secs(2), app.accept())
        .await
        .unwrap()
        .unwrap();
    tokio::join!(left.shutdown(), right.shutdown());
    for stream in [&mut stream, &mut service] {
        assert!(!matches!(
            timeout(Duration::from_secs(1), stream.read(&mut [0]))
                .await
                .unwrap(),
            Ok(1)
        ));
    }
    assert!(TcpStream::connect(address).await.is_err());
    for candidate in &reachable {
        assert!(TcpStream::connect(candidate).await.is_err());
    }
    assert!(left.state.lock().await.active.is_none());
    assert!(right.state.lock().await.active.is_none());
    eprintln!(
        "automatic selection: first candidate refused, {} authenticated host candidates, up={UP} down={DOWN}; control shutdown closed every listener",
        reachable.len()
    );
    drop(refused);
}

#[tokio::test]
async fn explicit_relay_does_not_require_candidate_discovery_or_pair_authentication() {
    let mut fixture = Fixture::start().await;
    let app = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target = app.local_addr().unwrap();
    let mut target_config = automatic(Role::Target, target);
    target_config.auto_candidates = Some(vec!["192.0.2.0/24".into()]);
    target_config.pair_key = STANDARD.encode([0x82; 32]);
    let left = fixture.attach(automatic(Role::Listen, target), true).await;
    let right = fixture.attach(target_config, true).await;
    let opened = fixture
        .call("ports.open", options(&left, &right, target, "relay"))
        .await
        .unwrap();
    assert_eq!(opened["transport"], "relay", "{opened}");
    roundtrip(
        &app,
        opened["listener"]["address"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap(),
    )
    .await;
    assert_eq!(
        fixture.wire_bytes.load(Ordering::SeqCst),
        (UP + DOWN) as u64
    );
    close(&fixture, &opened, (UP + DOWN) as u64).await;
}

#[tokio::test]
async fn empty_discovery_falls_back_or_fails_according_to_transport() {
    for mode in ["prefer_direct", "direct_only"] {
        let mut fixture = Fixture::start().await;
        let app = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target = app.local_addr().unwrap();
        let mut target_config = automatic(Role::Target, target);
        target_config.auto_candidates = Some(vec!["192.0.2.0/24".into()]);
        assert!(
            candidates::Networks::parse(target_config.auto_candidates.as_ref().unwrap())
                .unwrap()
                .gather()
                .unwrap()
                .is_empty(),
            "fixture requires no local TEST-NET-1 address"
        );
        let left = fixture.attach(automatic(Role::Listen, target), true).await;
        let right = fixture.attach(target_config, true).await;
        let result = fixture
            .call("ports.open", options(&left, &right, target, mode))
            .await;
        if mode == "direct_only" {
            assert!(result.unwrap_err().contains(UNREACHABLE));
            assert_eq!(
                fixture.call("ports.list", json!({})).await.unwrap()["ports"],
                json!([])
            );
            continue;
        }
        let opened = result.unwrap();
        assert_eq!(opened["transport"], "relay", "{opened}");
        assert_eq!(opened["fallback_reason"], UNREACHABLE);
        roundtrip(
            &app,
            opened["listener"]["address"]
                .as_str()
                .unwrap()
                .parse()
                .unwrap(),
        )
        .await;
        assert_eq!(
            fixture.wire_bytes.load(Ordering::SeqCst),
            (UP + DOWN) as u64
        );
        close(&fixture, &opened, (UP + DOWN) as u64).await;
    }
}

#[tokio::test]
async fn mixed_static_and_automatic_modes_select_relay_before_prepare() {
    for auto_listen in [true, false] {
        let mut fixture = Fixture::start().await;
        let app = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let held = TcpListener::bind("127.0.0.3:0").await.unwrap();
        let target = app.local_addr().unwrap();
        let candidate = held.local_addr().unwrap();
        let (left_config, mut right_config) = if auto_listen {
            (
                automatic(Role::Listen, target),
                config(Role::Target, target, candidate),
            )
        } else {
            (
                config(Role::Listen, target, candidate),
                automatic(Role::Target, target),
            )
        };
        right_config.pair_key = STANDARD.encode([0x82; 32]);
        let left = fixture.attach(left_config, true).await;
        let right = fixture.attach(right_config, true).await;
        let opened = fixture
            .call(
                "ports.open",
                options(&left, &right, target, "prefer_direct"),
            )
            .await
            .unwrap();
        assert_eq!(opened["transport"], "relay", "{opened}");
        assert!(opened["fallback_reason"].is_string());
        assert_no_connection(&held).await;
        roundtrip(
            &app,
            opened["listener"]["address"]
                .as_str()
                .unwrap()
                .parse()
                .unwrap(),
        )
        .await;
        close(&fixture, &opened, (UP + DOWN) as u64).await;
        assert!(
            fixture
                .call("ports.open", options(&left, &right, target, "direct_only"),)
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn empty_candidates_do_not_downgrade_invalid_pair_authentication() {
    let mut fixture = Fixture::start().await;
    let target = free("127.0.0.1").await;
    let mut target_config = automatic(Role::Target, target);
    target_config.auto_candidates = Some(vec!["192.0.2.0/24".into()]);
    target_config.pair_key = STANDARD.encode([0x82; 32]);
    let left = fixture.attach(automatic(Role::Listen, target), true).await;
    let right = fixture.attach(target_config, true).await;
    let error = fixture
        .call(
            "ports.open",
            options(&left, &right, target, "prefer_direct"),
        )
        .await
        .unwrap_err();
    assert!(error.contains("did not authenticate"), "{error}");
    assert_eq!(
        fixture.call("ports.list", json!({})).await.unwrap()["ports"],
        json!([])
    );
}

#[tokio::test]
async fn tampered_and_signed_forbidden_candidates_are_rejected_before_dial() {
    for (kind, peer_available) in [
        ("tampered", true),
        ("outside_policy", true),
        ("outside_policy", false),
        ("duplicate", true),
    ] {
        let app = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target = app.local_addr().unwrap();
        let trap_ip = if kind == "outside_policy" {
            "127.0.0.2:0"
        } else {
            "127.0.0.1:0"
        };
        let trap = TcpListener::bind(trap_ip).await.unwrap();
        let mut left_config = automatic(Role::Listen, target);
        left_config.auto_candidates = Some(vec!["127.0.0.1/32".into()]);
        let left = local("left", left_config);
        let right = local("right", automatic(Role::Target, target));
        let mapping = mapping("reject-candidate", target);
        let left_offer = prepare(&left, &mapping, Role::Listen).await;
        let mut right_offer = prepare(&right, &mapping, Role::Target).await;
        let trap_address = trap.local_addr().unwrap();
        right_offer.candidates = Some(if kind == "duplicate" {
            vec![trap_address, trap_address]
        } else {
            vec![trap_address]
        });
        if kind != "tampered" {
            resign(&right, &mapping, Role::Target, &mut right_offer);
        }
        let error = left
            .perform(Command::Activate {
                id: left_offer.id,
                peer: right_offer,
                peer_available,
            })
            .await
            .unwrap_err();
        assert_ne!(error, UNREACHABLE, "{kind} must fail closed");
        assert_no_connection(&trap).await;
        assert_no_connection(&app).await;
        assert!(left.state.lock().await.active.is_none());
        left.shutdown().await;
        right.shutdown().await;
    }
}

#[tokio::test]
async fn target_reservations_wait_for_authentication_and_reject_listener_candidates() {
    let target = free("127.0.0.1").await;
    let left = local("left", automatic(Role::Listen, target));
    let right = local("right", automatic(Role::Target, target));
    let mapping = mapping("listener-must-not-advertise", target);
    let mut left_offer = prepare(&left, &mapping, Role::Listen).await;
    assert_eq!(left_offer.candidates, Some(vec![]));
    let right_offer = prepare(&right, &mapping, Role::Target).await;
    let reserved = right_offer.candidates.as_ref().unwrap();
    assert!(!reserved.is_empty());
    for address in reserved {
        assert!(
            timeout(Duration::from_millis(200), TcpStream::connect(address))
                .await
                .unwrap()
                .is_err(),
            "candidate was listening before peer authorization"
        );
        assert!(TcpListener::bind(address).await.is_err());
    }
    left_offer.candidates = Some(vec![reserved[0]]);
    resign(&left, &mapping, Role::Listen, &mut left_offer);
    let error = right
        .perform(Command::Activate {
            id: right_offer.id.clone(),
            peer: left_offer,
            peer_available: true,
        })
        .await
        .unwrap_err();
    assert_ne!(error, UNREACHABLE);
    for address in reserved {
        let listener = TcpListener::bind(address).await.unwrap_or_else(|error| {
            panic!(
                "released candidate {address} could not be rebound: {error}; kind={:?}; errno={:?}",
                error.kind(),
                error.raw_os_error()
            )
        });
        drop(listener);
    }
    left.shutdown().await;
    right.shutdown().await;
}

#[tokio::test]
async fn expired_automatic_offers_release_the_reserved_sockets() {
    let target = free("127.0.0.1").await;
    let left = local("left", automatic(Role::Listen, target));
    let right = local("right", automatic(Role::Target, target));
    let mut mapping = mapping("expire-reservations", target);
    mapping.offer_until_ms = now_ms() + 250;
    let left_offer = prepare(&left, &mapping, Role::Listen).await;
    let right_offer = prepare(&right, &mapping, Role::Target).await;
    let reserved = right_offer.candidates.clone().unwrap();
    assert!(!reserved.is_empty());
    tokio::time::sleep(Duration::from_millis(350)).await;
    assert!(
        right
            .perform(Command::Activate {
                id: right_offer.id,
                peer: left_offer,
                peer_available: true,
            })
            .await
            .is_err()
    );
    for address in reserved {
        assert!(TcpListener::bind(address).await.is_ok());
    }
    left.shutdown().await;
    right.shutdown().await;
}

#[tokio::test]
async fn shutdown_during_a_stalled_tls_probe_is_bounded_and_releases_every_socket() {
    if crate::test_process::isolated("peer_port::tests::automatic::shutdown_during_a_stalled_tls_probe_is_bounded_and_releases_every_socket").await { return; }
    let app = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target = app.local_addr().unwrap();
    let trap = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let application_slot = tokio::net::TcpSocket::new_v4().unwrap();
    application_slot
        .bind("127.0.0.1:0".parse().unwrap())
        .unwrap();
    let application_address = application_slot.local_addr().unwrap();
    let configured = |role| {
        let mut config = automatic(role, target);
        config.listen = application_address;
        config
    };
    let left = local("left", configured(Role::Listen));
    let right = local("right", configured(Role::Target));
    let mut mapping = mapping("shutdown-stalled-probe", target);
    mapping.listen = application_address;
    let left_offer = prepare(&left, &mapping, Role::Listen).await;
    let mut right_offer = prepare(&right, &mapping, Role::Target).await;
    let reserved = right_offer.candidates.clone().unwrap();
    assert!(!reserved.is_empty());
    right_offer.candidates = Some(vec![trap.local_addr().unwrap()]);
    resign(&right, &mapping, Role::Target, &mut right_offer);
    let activating = left.clone();
    let activation = tokio::spawn(async move {
        activating
            .perform(Command::Activate {
                id: left_offer.id,
                peer: right_offer,
                peer_available: true,
            })
            .await
    });
    let (mut stalled, _) = timeout(Duration::from_secs(2), trap.accept())
        .await
        .expect("automatic probe should connect to the controlled TLS trap")
        .unwrap();
    // The trap keeps TCP open but never answers TLS. Shutdown must wait only
    // for the bounded probe, even though activation currently owns the state lock.
    timeout(Duration::from_secs(3), left.shutdown())
        .await
        .expect("stalled automatic probe must not indefinitely block control shutdown");
    assert!(
        timeout(Duration::from_secs(1), activation)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    let state = left.state.lock().await;
    assert!(state.closed && state.active.is_none() && state.pending.is_none());
    drop(state);
    let mut handshake = Vec::new();
    let _ = timeout(Duration::from_secs(1), stalled.read_to_end(&mut handshake))
        .await
        .expect("cancelled probe socket must close");
    assert_no_connection(&app).await;
    right.shutdown().await;
    for address in reserved {
        assert!(TcpListener::bind(address).await.is_ok());
    }
    drop(application_slot);
    let listener = TcpListener::bind(application_address).await.unwrap_or_else(|error| {
        panic!("released application slot {application_address} could not be rebound: {error}; kind={:?}; errno={:?}",
            error.kind(), error.raw_os_error())
    });
    drop(listener);
}

#[test]
fn automatic_policy_does_not_mix_static_or_bind_authority() {
    let target = "127.0.0.1:3000".parse().unwrap();
    let candidate = "127.0.0.2:44443".parse().unwrap();
    let mut mixed = automatic(Role::Target, target);
    mixed.candidate = Some(candidate);
    assert!(mixed.policy().is_err());
    let mut bind_override = automatic(Role::Target, target);
    bind_override.bind = Some(candidate);
    assert!(bind_override.policy().is_err());
    let mut missing = automatic(Role::Listen, target);
    missing.auto_candidates = None;
    assert!(missing.policy().is_err());
    let mut empty = automatic(Role::Listen, target);
    empty.auto_candidates = Some(vec![]);
    assert!(empty.policy().is_err());
}
