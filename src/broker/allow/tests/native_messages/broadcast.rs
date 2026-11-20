use super::*;

async fn broadcast(
    fixture: &Fixture,
    source: &Arc<Session>,
    token: &str,
    mut arguments: Value,
) -> Result<Value, String> {
    arguments
        .as_object_mut()
        .unwrap()
        .insert("client_id".into(), json!(source.client_id()));
    fixture
        .broker
        .message_request(IDENTITY_RESOURCE, token, arguments)
        .await
}

#[tokio::test]
async fn native_broadcast_returns_exact_client_and_thread_outcomes_without_duplicates() {
    let fixture = Fixture::new().await;
    let (source_socket, source) = connected(&fixture, Some("42"), true, vec![]).await;
    let (first_socket, first) = connected(&fixture, Some("42"), true, vec![]).await;
    let (second_socket, second) = connected(&fixture, Some("42"), true, vec![]).await;
    let (outsider_socket, outsider) = connected(&fixture, Some("7"), true, vec![]).await;
    let token = oauth(&fixture, IDENTITY_RESOURCE, "42");
    let authorization = Authorization::Identity(identity("42", "octocat"));
    let owner = crate::bridge::owner(IDENTITY_RESOURCE, &authorization).unwrap();
    let member = second
        .collaboration
        .dispatch(&owner, "join", json!({"name":"worker"}))
        .await
        .unwrap();
    let agent = member["agent_id"].as_str().unwrap();
    let agent_target = format!("agent:{}:{agent}", second.client_id());
    let selected = vec![
        destination(&first),
        agent_target.clone(),
        destination(&outsider),
    ];
    let args = json!({"action":"broadcast","recipient_ids":selected,
        "text":"explicit group @file $skill stays plain","request_id":"group-once"});
    let first_send = broadcast(&fixture, &source, &token, args.clone())
        .await
        .unwrap();
    assert_eq!(first_send["succeeded"], 2);
    assert_eq!(first_send["failed"], 1);
    assert_eq!(first_send["outcomes"][2]["ok"], false);
    assert!(
        first_send["outcomes"][2]["error"]
            .as_str()
            .unwrap()
            .contains("unavailable")
    );
    let (retry_a, retry_b) = tokio::join!(
        broadcast(&fixture, &source, &token, args.clone()),
        broadcast(&fixture, &source, &token, args.clone())
    );
    for retried in [retry_a.unwrap(), retry_b.unwrap()] {
        for index in 0..2 {
            assert_eq!(
                retried["outcomes"][index]["receipt"]["message_id"],
                first_send["outcomes"][index]["receipt"]["message_id"]
            );
            assert_eq!(retried["outcomes"][index]["receipt"]["duplicate"], true);
        }
    }
    let mut reordered = args.clone();
    reordered["recipient_ids"].as_array_mut().unwrap().reverse();
    let reordered = broadcast(&fixture, &source, &token, reordered)
        .await
        .unwrap();
    assert_eq!(
        reordered["outcomes"][2]["receipt"]["message_id"],
        first_send["outcomes"][0]["receipt"]["message_id"]
    );
    let mut changed = args.clone();
    changed["text"] = json!("changed text");
    assert!(
        broadcast(&fixture, &source, &token, changed)
            .await
            .unwrap_err()
            .contains("different")
    );
    let mut changed = args.clone();
    changed["recipient_ids"][0] = json!(destination(&source));
    assert!(
        broadcast(&fixture, &source, &token, changed)
            .await
            .unwrap_err()
            .contains("different")
    );
    let first_delivery =
        fixture
            .broker
            .include_native_messages(&first, &authorization, None, 32, 100_000);
    assert_eq!(first_delivery.len(), 1);
    assert_eq!(first_delivery[0]["sender"]["kind"], "agent");
    assert_eq!(first_delivery[0]["text"], args["text"]);
    assert!(
        fixture
            .broker
            .include_native_messages(&second, &authorization, None, 32, 100_000)
            .is_empty()
    );
    let exact =
        fixture
            .broker
            .include_native_messages(&second, &authorization, Some(agent), 32, 100_000);
    assert_eq!(exact.len(), 1);
    assert_eq!(exact[0]["recipient"]["id"], agent_target);
    assert!(
        fixture
            .broker
            .include_native_messages(&source, &authorization, None, 32, 100_000)
            .is_empty()
    );
    for socket in [source_socket, first_socket, second_socket, outsider_socket] {
        close(socket).await;
    }
}

#[tokio::test]
async fn native_broadcast_retries_failed_mailboxes_and_never_resends_evicted_acceptances() {
    let fixture = Fixture::new().await;
    let (source_socket, source) = connected(&fixture, Some("42"), true, vec![]).await;
    let (first_socket, first) = connected(&fixture, Some("42"), true, vec![]).await;
    let (full_socket, full) = connected(&fixture, Some("42"), true, vec![]).await;
    let token = oauth(&fixture, IDENTITY_RESOURCE, "42");
    for index in 0..messages::MAX_MESSAGES {
        remote(
            &fixture,
            &source,
            IDENTITY_RESOURCE,
            &token,
            json!({
                "action":"send","recipient_id":destination(&full),"text":"capacity fixture",
                "request_id":format!("fill-full-{index}")
            }),
        )
        .await
        .unwrap();
    }
    let args = json!({"action":"broadcast","recipient_ids":[destination(&first),destination(&full)],
        "text":"broadcast payload","request_id":"partial-group"});
    let partial = broadcast(&fixture, &source, &token, args.clone())
        .await
        .unwrap();
    assert_eq!(partial["succeeded"], 1);
    assert!(
        partial["outcomes"][1]["error"]
            .as_str()
            .unwrap()
            .contains("full")
    );
    for _ in 0..messages::MAX_MESSAGES / messages::MAX_DELIVERY {
        let received = remote(
            &fixture,
            &full,
            IDENTITY_RESOURCE,
            &token,
            json!({"action":"inbox"}),
        )
        .await
        .unwrap();
        assert_eq!(received.messages.len(), messages::MAX_DELIVERY);
        for message in received.messages {
            remote(
                &fixture,
                &full,
                IDENTITY_RESOURCE,
                &token,
                json!({
                    "action":"ack","message_id":message.receipt.message_id
                }),
            )
            .await
            .unwrap();
        }
    }
    let retried = broadcast(&fixture, &source, &token, args.clone())
        .await
        .unwrap();
    assert_eq!(retried["succeeded"], 2);
    assert_eq!(retried["outcomes"][0]["receipt"]["duplicate"], true);
    assert_eq!(retried["outcomes"][1]["receipt"]["duplicate"], false);
    let received = remote(
        &fixture,
        &first,
        IDENTITY_RESOURCE,
        &token,
        json!({"action":"inbox"}),
    )
    .await
    .unwrap();
    assert_eq!(received.messages.len(), 1);
    remote(
        &fixture,
        &first,
        IDENTITY_RESOURCE,
        &token,
        json!({
            "action":"ack","message_id":received.messages[0].receipt.message_id
        }),
    )
    .await
    .unwrap();
    for index in 0..messages::MAX_MESSAGES {
        remote(
            &fixture,
            &source,
            IDENTITY_RESOURCE,
            &token,
            json!({
                "action":"send","recipient_id":destination(&first),"text":"later message",
                "request_id":format!("fill-first-{index}")
            }),
        )
        .await
        .unwrap();
    }
    let retained = broadcast(&fixture, &source, &token, args).await.unwrap();
    assert_eq!(retained["outcomes"][0]["ok"], false);
    assert!(
        retained["outcomes"][0]["error"]
            .as_str()
            .unwrap()
            .contains("not resent")
    );
    assert_eq!(
        retained["outcomes"][1]["receipt"]["message_id"],
        retried["outcomes"][1]["receipt"]["message_id"]
    );
    assert!(remote(&fixture, &source, IDENTITY_RESOURCE, &token, json!({
        "action":"send","recipient_id":destination(&first),"text":"single reuse","request_id":"partial-group"
    })).await.unwrap_err().contains("broadcast"));
    for socket in [source_socket, first_socket, full_socket] {
        close(socket).await;
    }
}
