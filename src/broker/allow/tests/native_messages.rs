//! Real websocket/device registration plus normal fixture OAuth grants.
use super::*;
use crate::wire::messages::{self, Command, Origin, Reply, Status};

async fn connected(
    fixture: &Fixture,
    subject: Option<&str>,
    offers_messages: bool,
    allow: Vec<String>,
) -> (Socket, Arc<Session>) {
    let token = match subject {
        Some(subject) => Some(fixture.token(subject).await),
        None => None,
    };
    let mut request = fixture.request(token.as_deref(), true);
    if offers_messages {
        request.headers_mut().insert(
            wire::CAPABILITIES_HEADER,
            format!(
                "{}, {}, {}",
                wire::SESSION_ACCESS_CAPABILITY,
                wire::CONTROL_ATTRIBUTION_CAPABILITY,
                messages::CAPABILITY
            )
            .parse()
            .unwrap(),
        );
    }
    let (mut socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    send(
        &mut socket,
        Frame::Hello {
            version: wire::VERSION,
            mode: Mode::Shell,
            metadata: ClientMetadata {
                name: "same visible name".into(),
                title: Some("same visible name".into()),
                cwd: "/fixture".into(),
                hostname: "localhost".into(),
                allow,
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
    assert_eq!(
        capabilities.iter().any(|v| v == messages::CAPABILITY),
        offers_messages && subject.is_some()
    );
    let session = fixture
        .broker
        .sessions
        .read()
        .await
        .values()
        .find_map(|sessions| sessions.get(&client_id))
        .unwrap()
        .clone();
    (socket, session)
}

async fn local(socket: &mut Socket, id: u64, command: Command) -> Result<Reply, String> {
    send(socket, Frame::Messages { id, command }).await;
    let Frame::MessagesReply { id: actual, result } = receive(socket).await else {
        panic!("native receipt required");
    };
    assert_eq!(actual, id);
    result
}

fn oauth(fixture: &Fixture, user: &str, subject: &str) -> String {
    fixture.broker.oauth.test_identity_tokens(user, subject)["access_token"]
        .as_str()
        .unwrap()
        .to_owned()
}

async fn remote(
    fixture: &Fixture,
    source: &Arc<Session>,
    user: &str,
    token: &str,
    mut arguments: Value,
) -> Result<Reply, String> {
    arguments
        .as_object_mut()
        .unwrap()
        .insert("client_id".into(), json!(source.client_id()));
    let result = fixture
        .broker
        .message_request(user, token, arguments)
        .await?;
    serde_json::from_value(result).map_err(|e| e.to_string())
}

fn destination(session: &Session) -> String {
    format!("client:{}", session.client_id())
}

#[tokio::test]
async fn native_device_negotiation_exact_recipient_and_receipts_use_real_websockets() {
    let fixture = Fixture::new().await;
    let (old_socket, _) = connected(&fixture, Some("42"), false, vec![]).await;
    let (anonymous_socket, anonymous) = connected(&fixture, None, true, names(&["alice"])).await;
    assert!(
        fixture
            .broker
            .local_message_command(&anonymous, None, Command::List {})
            .await
            .is_err()
    );
    let (mut sender, source) = connected(&fixture, Some("42"), true, vec![]).await;
    let (mut receiver, target) = connected(&fixture, Some("42"), true, vec![]).await;
    let (mut outsider_socket, outsider) = connected(&fixture, Some("7"), true, vec![]).await;
    assert_ne!(source.client_id(), target.client_id());
    let list = local(&mut sender, 1, Command::List {}).await.unwrap();
    let own: Vec<_> = list
        .recipients
        .iter()
        .filter(|r| r.account.subject == "42")
        .collect();
    assert!(own.iter().any(|r| r.id == destination(&source)));
    assert!(own.iter().any(|r| r.id == destination(&target)));
    assert!(own.iter().all(|r| r.display_name == "same visible name"));
    assert!(
        !list
            .recipients
            .iter()
            .any(|r| r.client_id == outsider.client_id())
    );
    let other = local(&mut outsider_socket, 1, Command::List {})
        .await
        .unwrap();
    assert!(
        !other
            .recipients
            .iter()
            .any(|r| r.client_id == target.client_id())
    );
    let body = "@not-a-file !not-a-command\n纯文本原样保留";
    let sent = local(
        &mut sender,
        2,
        Command::Send {
            recipient_id: destination(&target),
            text: body.into(),
            request_id: "literal-text".into(),
        },
    )
    .await
    .unwrap();
    assert_eq!(sent.receipts.len(), 1);
    assert_eq!(sent.receipts[0].status, Status::Queued);
    let mid = sent.receipts[0].message_id.clone();
    assert!(
        local(&mut sender, 3, Command::Inbox {})
            .await
            .unwrap()
            .messages
            .is_empty()
    );
    let received = local(&mut receiver, 1, Command::Inbox {}).await.unwrap();
    assert_eq!(received.messages.len(), 1);
    let message = &received.messages[0];
    assert_eq!(message.text, body);
    assert_eq!(message.sender.kind, Origin::LocalUser);
    assert_eq!(message.sender.account.subject, "42");
    assert!(message.sender.agent.is_none());
    assert_eq!(message.recipient.id, destination(&target));
    assert_eq!(message.receipt.status, Status::Included);
    let ack = local(
        &mut receiver,
        2,
        Command::Ack {
            message_id: mid.clone(),
        },
    )
    .await
    .unwrap();
    assert_eq!(ack.receipts[0].status, Status::Acknowledged);
    assert_eq!(
        local(&mut sender, 4, Command::Status { message_id: mid })
            .await
            .unwrap()
            .receipts[0]
            .status,
        Status::Acknowledged
    );
    for socket in [
        old_socket,
        anonymous_socket,
        sender,
        receiver,
        outsider_socket,
    ] {
        close(socket).await;
    }
}

#[tokio::test]
async fn native_remote_origin_and_idempotency_cannot_be_forged_by_labels() {
    let fixture = Fixture::new().await;
    let (sender_socket, source) = connected(&fixture, Some("42"), true, vec![]).await;
    let (target_socket, target) = connected(&fixture, Some("42"), true, vec![]).await;
    let token = oauth(&fixture, IDENTITY_RESOURCE, "42");
    let args = json!({"action":"send","recipient_id":destination(&target),
        "text":"agent text","request_id":"same-request"});
    let first = remote(&fixture, &source, IDENTITY_RESOURCE, &token, args.clone())
        .await
        .unwrap();
    let repeated = remote(&fixture, &source, IDENTITY_RESOURCE, &token, args.clone())
        .await
        .unwrap();
    assert_eq!(
        first.receipts[0].message_id,
        repeated.receipts[0].message_id
    );
    assert!(repeated.receipts[0].duplicate);
    for forged in [
        json!({"action":"send","recipient_id":destination(&target),"text":"x",
            "request_id":"forged","sender_kind":"local_user"}),
        json!({"action":"send","recipient_id":"same visible name","text":"x","request_id":"label"}),
        json!({"action":"send","recipient_id":destination(&source),
            "text":"agent text","request_id":"same-request"}),
    ] {
        assert!(
            remote(&fixture, &source, IDENTITY_RESOURCE, &token, forged)
                .await
                .is_err()
        );
    }
    assert!(
        fixture
            .broker
            .include_native_messages(
                &target,
                &Authorization::Identity(identity("7", "alice")),
                None,
                32,
                100_000
            )
            .is_empty()
    );
    let delivered = fixture.broker.include_native_messages(
        &target,
        &Authorization::Identity(identity("42", "renamed")),
        None,
        32,
        100_000,
    );
    assert_eq!(delivered.len(), 1);
    assert_eq!(delivered[0]["sender"]["kind"], "agent");
    assert_eq!(delivered[0]["sender"]["account"]["subject"], "42");
    assert!(delivered[0]["sender"]["agent"].is_null());
    assert!(delivered[0].get("user_messages").is_none());
    assert!(
        fixture
            .broker
            .include_native_messages(
                &target,
                &Authorization::Identity(identity("42", "renamed")),
                None,
                32,
                100_000
            )
            .is_empty()
    );
    close(sender_socket).await;
    close(target_socket).await;
}

#[tokio::test]
async fn native_agent_mailbox_needs_exact_live_credentials_even_with_same_name() {
    let fixture = Fixture::new().await;
    let (mut sender, source) = connected(&fixture, Some("42"), true, vec![]).await;
    let (target_socket, target) = connected(&fixture, Some("42"), true, vec![]).await;
    let authorization = Authorization::Identity(identity("42", "octocat"));
    let owner = crate::bridge::owner(IDENTITY_RESOURCE, &authorization).unwrap();
    let one = target
        .collaboration
        .dispatch(&owner, "join", json!({"name":"worker"}))
        .await
        .unwrap();
    let two = target
        .collaboration
        .dispatch(&owner, "join", json!({"name":"worker"}))
        .await
        .unwrap();
    let recipient = format!(
        "agent:{}:{}",
        target.client_id(),
        one["agent_id"].as_str().unwrap()
    );
    let sent = local(
        &mut sender,
        1,
        Command::Send {
            recipient_id: recipient.clone(),
            text: "exact worker only".into(),
            request_id: "agent-specific".into(),
        },
    )
    .await
    .unwrap();
    let mid = sent.receipts[0].message_id.clone();
    let token = oauth(&fixture, IDENTITY_RESOURCE, "42");
    assert!(
        remote(
            &fixture,
            &target,
            IDENTITY_RESOURCE,
            &token,
            json!({"action":"inbox"})
        )
        .await
        .unwrap()
        .messages
        .is_empty()
    );
    assert!(
        remote(
            &fixture,
            &target,
            IDENTITY_RESOURCE,
            &token,
            json!({"action":"inbox",
        "agent_id":one["agent_id"],"agent_token":two["token"]})
        )
        .await
        .is_err()
    );
    let got = remote(
        &fixture,
        &target,
        IDENTITY_RESOURCE,
        &token,
        json!({"action":"inbox",
        "agent_id":one["agent_id"],"agent_token":one["token"]}),
    )
    .await
    .unwrap();
    assert_eq!(got.messages.len(), 1);
    assert_eq!(got.messages[0].recipient.id, recipient);
    assert!(
        remote(
            &fixture,
            &target,
            IDENTITY_RESOURCE,
            &token,
            json!({"action":"ack","message_id":mid,
        "agent_id":two["agent_id"],"agent_token":two["token"]})
        )
        .await
        .is_err()
    );
    let ack = remote(
        &fixture,
        &target,
        IDENTITY_RESOURCE,
        &token,
        json!({"action":"ack","message_id":mid,
        "agent_id":one["agent_id"],"agent_token":one["token"]}),
    )
    .await
    .unwrap();
    assert_eq!(ack.receipts[0].status, Status::Acknowledged);
    assert!(
        fixture
            .broker
            .include_native_messages(&source, &authorization, None, 32, 100_000)
            .is_empty()
    );
    close(sender).await;
    close(target_socket).await;
}

#[tokio::test]
async fn native_revoked_sender_acl_expires_pending_text_without_delivery() {
    let fixture = Fixture::new().await;
    let (mut sender, _) = connected(&fixture, Some("42"), true, vec![]).await;
    let (mut receiver, target) =
        connected(&fixture, Some("7"), true, names(&["alice", "colleague"])).await;
    local(
        &mut sender,
        1,
        Command::Send {
            recipient_id: destination(&target),
            text: "must not arrive after revoke".into(),
            request_id: "revoke".into(),
        },
    )
    .await
    .unwrap();
    send(
        &mut receiver,
        Frame::Access {
            id: 10,
            command: AccessCommand::Remove {
                principal: "alice@ssmcp.test".into(),
            },
        },
    )
    .await;
    let Frame::AccessReply {
        id: 10,
        result: Ok(_),
    } = receive(&mut receiver).await
    else {
        panic!("owner ACL removal required");
    };
    let received = local(&mut receiver, 11, Command::Inbox {}).await.unwrap();
    assert!(received.messages.is_empty());
    let outgoing = local(&mut sender, 2, Command::List {}).await.unwrap();
    assert_eq!(outgoing.receipts.len(), 1);
    assert_eq!(outgoing.receipts[0].status, Status::Expired);
    assert!(
        local(
            &mut sender,
            3,
            Command::Send {
                recipient_id: destination(&target),
                text: "still denied".into(),
                request_id: "after-revoke".into()
            }
        )
        .await
        .is_err()
    );
    close(sender).await;
    close(receiver).await;
}

#[tokio::test]
async fn native_scoped_oauth_never_expands_into_all_account_resources() {
    let fixture = Fixture::new().await;
    let (source_socket, source) = connected(&fixture, Some("42"), true, vec![]).await;
    let (target_socket, target) =
        connected(&fixture, Some("7"), true, names(&["alice", "colleague"])).await;
    assert_ne!(source.ssh_user, target.ssh_user);
    let token = oauth(&fixture, &source.ssh_user, "42");
    let listed = remote(
        &fixture,
        &source,
        &source.ssh_user,
        &token,
        json!({"action":"list"}),
    )
    .await
    .unwrap();
    assert!(
        listed
            .recipients
            .iter()
            .any(|r| r.client_id == source.client_id())
    );
    assert!(
        !listed
            .recipients
            .iter()
            .any(|r| r.client_id == target.client_id())
    );
    assert!(remote(&fixture,&source,&source.ssh_user,&token,json!({"action":"send",
        "recipient_id":destination(&target),"text":"outside resource","request_id":"resource-bound"}))
        .await.is_err());
    close(source_socket).await;
    close(target_socket).await;
}
