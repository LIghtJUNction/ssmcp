use super::*;
use crate::events::{Actor, Filter};
use crate::wire::repository_events::{CAPABILITY, Counts, Event};

#[tokio::test]
async fn negotiated_repository_events_reach_only_currently_allowed_accounts() {
    let fixture = Fixture::new().await;
    let mut request = fixture.request(None, true);
    request.headers_mut().insert(
        wire::CAPABILITIES_HEADER,
        format!(
            "{},{},{},{}",
            wire::SESSION_ACCESS_CAPABILITY,
            wire::CONTROL_ATTRIBUTION_CAPABILITY,
            wire::CHAT_DISABLED_CAPABILITY,
            CAPABILITY
        )
        .parse()
        .unwrap(),
    );
    let (mut socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    send(
        &mut socket,
        legacy_self_hello(names(&["alice", "colleague"])),
    )
    .await;
    let Frame::Ready {
        client_id,
        capabilities,
        ..
    } = receive(&mut socket).await
    else {
        panic!("expected ready")
    };
    assert!(capabilities.iter().any(|cap| cap == CAPABILITY));
    assert!(
        capabilities
            .iter()
            .any(|cap| cap == wire::CHAT_DISABLED_CAPABILITY)
    );

    let hub = &fixture.broker.events;
    let subscribe = |subject: &str| {
        let owner = crate::bridge::owner(
            IDENTITY_RESOURCE,
            &Authorization::Identity(identity(subject, "observer")),
        )
        .unwrap();
        let actor = Actor {
            owner,
            client_id: client_id.clone(),
            agent_id: "observer".into(),
        };
        let id = hub
            .subscribe(
                &actor,
                "repo",
                Filter {
                    kinds: vec!["repo.*".into()],
                    ..Default::default()
                },
                None,
            )
            .unwrap()["subscription_id"]
            .as_str()
            .unwrap()
            .to_owned();
        (actor, id)
    };
    let (alice, a) = subscribe("42");
    let (colleague, b) = subscribe("7");
    let (outsider, c) = subscribe("123");
    send(
        &mut socket,
        Frame::RepositoryEvent {
            event: Event::UpstreamChanged {
                branch: "main".into(),
                upstream: "origin/main".into(),
                ahead: 1,
                behind: 4,
                previous: Some(Counts {
                    ahead: 1,
                    behind: 0,
                }),
            },
        },
    )
    .await;
    // Each AccessReply is a barrier after the preceding one-way event frame.
    send(
        &mut socket,
        Frame::Access {
            id: 1,
            command: AccessCommand::Remove {
                principal: "colleague@ssmcp.test".into(),
            },
        },
    )
    .await;
    assert!(matches!(
        receive(&mut socket).await,
        Frame::AccessReply {
            id: 1,
            result: Ok(_)
        }
    ));
    send(
        &mut socket,
        Frame::RepositoryEvent {
            event: Event::PullRequestOpened {
                remote: "origin".into(),
                number: 19,
            },
        },
    )
    .await;
    send(
        &mut socket,
        Frame::Access {
            id: 2,
            command: AccessCommand::List {},
        },
    )
    .await;
    assert!(matches!(
        receive(&mut socket).await,
        Frame::AccessReply {
            id: 2,
            result: Ok(_)
        }
    ));
    let page = hub.read(&alice, &a, 8).unwrap();
    let events = page["events"].as_array().unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0]["kind"], "repo.upstream_changed");
    assert_eq!(events[0]["data"]["behind"], 4);
    assert_eq!(events[0]["data"]["previous"]["behind"], 0);
    assert_eq!(events[1]["kind"], "repo.pull_request_opened");
    assert_eq!(events[1]["origin"], "system");
    assert_eq!(events[1]["client_id"], client_id);
    assert_eq!(events[1]["data"], json!({"remote":"origin","number":19}));
    assert_eq!(hub.read(&alice, &a, 8).unwrap()["events"], page["events"]);
    hub.ack(&alice, &a, page["next_cursor"].as_u64().unwrap())
        .unwrap();
    assert_eq!(hub.read(&alice, &a, 8).unwrap()["events"], json!([]));
    assert_eq!(
        hub.read(&colleague, &b, 8).unwrap()["events"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(hub.read(&outsider, &c, 8).unwrap()["events"], json!([]));
    close(socket).await;
}

#[tokio::test]
async fn credentialed_passive_client_only_receives_its_explicit_repository_capability() {
    let fixture = Fixture::new().await;
    let token = fixture.token("42").await;
    for (offer, enabled) in [
        (wire::CHAT_DISABLED_CAPABILITY.to_owned(), false),
        (
            format!("{},{}", wire::CHAT_DISABLED_CAPABILITY, CAPABILITY),
            true,
        ),
    ] {
        let mut request = fixture.request(Some(&token), false);
        request
            .headers_mut()
            .insert(wire::CAPABILITIES_HEADER, offer.parse().unwrap());
        let (mut socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
        // Existing self-only clients bind the credential subject even if the old
        // GitHub label is present. Enabling observation cannot enable delegation.
        send(
            &mut socket,
            legacy_self_hello(vec!["octocat@github".into()]),
        )
        .await;
        let Frame::Ready { capabilities, .. } = receive(&mut socket).await else {
            panic!("expected Ready");
        };
        assert_eq!(capabilities.iter().any(|cap| cap == CAPABILITY), enabled);
        assert!(
            !capabilities
                .iter()
                .any(|cap| cap == wire::SESSION_ACCESS_CAPABILITY)
        );
        close(socket).await;
    }
}
