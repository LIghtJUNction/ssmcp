use super::*;

#[tokio::test]
async fn git_tracking_frame_reaches_the_public_subscription() {
    let fixture = Fixture::new().await;
    let mut request = fixture.request(None, true);
    let offer = format!(
        "{},{},{}",
        wire::SESSION_ACCESS_CAPABILITY,
        wire::CONTROL_ATTRIBUTION_CAPABILITY,
        wire::GIT_TRACKING_CAPABILITY
    );
    request
        .headers_mut()
        .insert(wire::CAPABILITIES_HEADER, offer.parse().unwrap());
    let (mut socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    send(
        &mut socket,
        Frame::Hello {
            version: wire::VERSION,
            mode: Mode::Shell,
            metadata: ClientMetadata {
                name: "git fixture".into(),
                title: None,
                cwd: "/fixture".into(),
                hostname: "local".into(),
                allow: names(&["alice"]),
            },
        },
    )
    .await;
    let Frame::Ready {
        client_id,
        capabilities,
        ..
    } = receive(&mut socket).await
    else {
        panic!("ready required");
    };
    assert!(
        capabilities
            .iter()
            .any(|cap| cap == wire::GIT_TRACKING_CAPABILITY)
    );
    assert!(
        !capabilities
            .iter()
            .any(|cap| cap == wire::repository_events::CAPABILITY)
    );
    let auth = Authorization::Identity(identity("42", "octocat"));
    let session = fixture
        .broker
        .client("mcp", &auth, &client_id)
        .await
        .unwrap();
    let owner = crate::bridge::owner("mcp", &auth).unwrap();
    let member = session
        .collaboration
        .dispatch(&owner, "join", json!({"name":"observer"}))
        .await
        .unwrap();
    let token = fixture.broker.oauth.test_identity_tokens("mcp", "42")["access_token"]
        .as_str()
        .unwrap()
        .to_owned();
    let events = |mut args: Value| {
        args["client_id"] = json!(client_id);
        args["agent_id"] = member["agent_id"].clone();
        args["token"] = member["token"].clone();
        crate::http::events_tool::dispatch(
            fixture.broker.clone(),
            "mcp".into(),
            token.clone(),
            args,
        )
    };
    let sub = events(
        json!({"action":"subscribe","request_id":"git-tracking","filter":{"kinds":["git.*"]}}),
    )
    .await
    .unwrap();
    let id = &sub["subscription_id"];
    let change = wire::GitTrackingChange {
        branch: "main".into(),
        upstream: "refs/remotes/origin/main".into(),
        head: "a".repeat(40),
        previous_upstream_oid: "b".repeat(40),
        upstream_oid: "c".repeat(40),
        ahead: 1,
        behind: 2,
    };
    for unsafe_name in [
        "https://user:credential@invalid.example/repo",
        "refs/remotes/origin/ghp_fixtureSecret",
    ] {
        let mut invalid = change.clone();
        invalid.upstream = unsafe_name.into();
        send(&mut socket, Frame::GitTracking { change: invalid }).await;
    }
    send(
        &mut socket,
        Frame::GitTracking {
            change: change.clone(),
        },
    )
    .await;
    let page = events(json!({"action":"wait","subscription_id":id,"wait_ms":2000}))
        .await
        .unwrap();
    let entries = page["events"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["client_id"], client_id);
    assert_eq!(entries[0]["kind"], "git.tracking_changed");
    assert_eq!(entries[0]["origin"], "system");
    assert_eq!(entries[0]["data"], json!(change));
    events(json!({"action":"ack","subscription_id":id,"through":page["next_cursor"]}))
        .await
        .unwrap();
    assert_eq!(
        events(json!({"action":"read","subscription_id":id}))
            .await
            .unwrap()["events"],
        json!([])
    );
}
