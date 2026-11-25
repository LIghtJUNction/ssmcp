use super::*;

/// Send list through the public MCP handler on this fixture's loopback router.
async fn list(f: &Fixture, token: &str, arguments: Value) -> Value {
    crate::http_client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap()
        .post(format!("{}/mcp", f.base))
        .bearer_auth(token)
        .header("accept", "application/json")
        .json(&json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
                        "params":{"name":"list","arguments":arguments}}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap()
}

#[tokio::test]
async fn verified_project_predecessor_is_discovery_metadata_in_the_original_scope() {
    let f = Fixture::new().await;
    let token = |subject| {
        f.broker.oauth.test_identity_tokens("mcp", subject)["access_token"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let owner = token("42");
    let outsider = token("123");
    let (mut before, old) = open(&f, &["alice", "colleague"]).await;
    prove(&mut before, signed(&old, None)).await;
    disconnect(before, &old).await;
    let (mut after, new) = open(&f, &["colleague", "alice"]).await;
    let stale = json!({"client_id":old.client_id(), "cursor":"obsolete-catalog-cursor"});

    let unverified = list(&f, &owner, stale.clone()).await;
    assert_eq!(unverified["result"]["isError"], true);
    prove(&mut after, signed(&new, Some(&old))).await;
    let plain = list(&f, &owner, json!({})).await;
    let hints = plain["result"]["structuredContent"]["handoffs"]
        .as_array()
        .unwrap();
    assert_eq!(hints.len(), 1);
    assert_eq!(hints[0]["status"], "same_project");
    assert_eq!(hints[0]["previous_client_id"], old.client_id());
    assert_eq!(hints[0]["client_id"], new.client_id());
    assert!(hints[0].get("device_id").is_none());

    // Nobody is serving tools on this socket. A catalog dispatch would time out.
    let page = list(&f, &owner, stale.clone()).await;
    let page = &page["result"]["structuredContent"];
    assert_eq!(page["handoff"], hints[0]);
    assert_eq!(page["tools"], json!([]));
    assert_eq!(page["nextCursor"], Value::Null);
    let address_only = list(
        &f,
        &owner,
        json!({"device_id":format!("dev_{}", "0".repeat(64))}),
    )
    .await;
    assert_eq!(
        address_only["result"]["structuredContent"]["clients"],
        json!([])
    );
    assert!(!address_only.to_string().contains(new.client_id()));
    let denied = list(&f, &outsider, stale.clone()).await;
    assert_eq!(denied["result"]["isError"], true);
    assert!(!denied.to_string().contains(new.client_id()));

    send(
        &mut after,
        Frame::Access {
            id: 9,
            command: AccessCommand::Remove {
                principal: "colleague@ssmcp.test".into(),
            },
        },
    )
    .await;
    assert!(matches!(
        receive(&mut after).await,
        Frame::AccessReply {
            id: 9,
            result: Ok(_)
        }
    ));
    let changed = list(&f, &owner, stale).await;
    assert_eq!(changed["result"]["isError"], true);
    assert!(!changed.to_string().contains(new.client_id()));
}
