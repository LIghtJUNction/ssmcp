use super::*;
use serde_json::json;

fn identity(subject: &str, login: &str) -> Identity {
    Identity {
        provider: "github".into(),
        subject: subject.into(),
        login: login.into(),
    }
}

fn fixture() -> (Arc<Session>, mpsc::Receiver<Call>) {
    let alice = identity("123", "alice");
    let mut session = tests::allowed_session(
        "attribution-room",
        "fixture",
        vec![alice.clone(), identity("456", "bob")],
    );
    let (sender, receiver) = mpsc::channel(4);
    let mutable = Arc::get_mut(&mut session).unwrap();
    mutable.sender = sender;
    mutable.slots = Semaphore::new(4);
    mutable.owner_subject = Some(alice.subject.clone());
    mutable.owner_identity = Some(alice);
    (session, receiver)
}

fn operation(name: &str) -> Operation {
    Operation::CallTool {
        name: name.into(),
        arguments: json!({}),
    }
}

async fn bound_run(
    session: &Session,
    account: &Identity,
) -> (crate::code_mode::AuditRun, String, String) {
    let authorization = Authorization::Identity(account.clone());
    let owner = crate::bridge::owner("mcp", &authorization).unwrap();
    let agent = session
        .collaboration
        .dispatch(
            &owner,
            "join",
            json!({"name":format!("{} worker", account.login)}),
        )
        .await
        .unwrap();
    let id = agent["agent_id"].as_str().unwrap().to_owned();
    let token = agent["token"].as_str().unwrap().to_owned();
    let run =
        crate::code_mode::AuditRun::new("return await call(clientId, 'fixture.tool', {});", None)
            .unwrap();
    run.bind(session.client_id(), &owner, &id, &token).unwrap();
    (run, id, token)
}

#[tokio::test]
async fn control_attribution_keeps_concurrent_accounts_agents_and_owner_separate() {
    let (session, mut receiver) = fixture();
    let mut requests = Vec::new();
    let mut expected = HashMap::new();
    for account in [identity("123", "alice"), identity("456", "bob")] {
        let (run, agent, token) = bound_run(&session, &account).await;
        let tool = format!("fixture.{}", account.login);
        expected.insert(tool.clone(), (account.subject.clone(), agent, token));
        let target = session.clone();
        requests.push(tokio::spawn(crate::code_mode::AUDIT_RUN.scope(
            run,
            async move {
                target
                    .call_with_authorization(
                        operation(&tool),
                        Some(Controller {
                            name: Some("alice (caller claim)".into()),
                            model: Some("claimed-model".into()),
                            source: wire::ControllerSource::CallerReported,
                        }),
                        &Authorization::Identity(account),
                    )
                    .await
            },
        )));
    }
    let first = receiver.recv().await.unwrap();
    let second = receiver.recv().await.unwrap();
    for call in [second, first] {
        let (subject, agent, token) = &expected[call.operation.name()];
        let attribution = call.attribution.as_ref().unwrap();
        assert_eq!(&attribution.account.subject, subject);
        assert_eq!(&attribution.agent.as_ref().unwrap().id, agent);
        assert_eq!(attribution.owner.as_ref().unwrap().subject, "123");
        assert!(!serde_json::to_string(attribution).unwrap().contains(token));
        call.reply.send(Ok(json!({"ok":true}))).unwrap();
    }
    for request in requests {
        assert!(request.await.unwrap().is_ok());
    }
    let target = session.clone();
    let request = tokio::spawn(async move {
        target
            .call_with_authorization(
                operation("fixture.unbound"),
                None,
                &Authorization::Identity(identity("456", "bob")),
            )
            .await
    });
    let call = receiver.recv().await.unwrap();
    assert!(call.attribution.as_ref().unwrap().agent.is_none());
    assert_eq!(call.attribution.as_ref().unwrap().account.subject, "456");
    call.reply.send(Ok(json!({}))).unwrap();
    request.await.unwrap().unwrap();
}

#[tokio::test]
async fn control_attribution_rejects_cross_account_and_expired_agent_credentials() {
    let (session, mut receiver) = fixture();
    let (run, id, token) = bound_run(&session, &identity("456", "bob")).await;
    let result = crate::code_mode::AUDIT_RUN
        .scope(
            run.clone(),
            session.call_with_authorization(
                operation("fixture.forged"),
                None,
                &Authorization::Identity(identity("123", "alice")),
            ),
        )
        .await;
    assert!(
        result
            .unwrap_err()
            .contains("another authenticated account")
    );
    assert!(receiver.try_recv().is_err());
    let owner =
        crate::bridge::owner("mcp", &Authorization::Identity(identity("456", "bob"))).unwrap();
    session
        .collaboration
        .dispatch(&owner, "leave", json!({"agent_id":id,"token":token}))
        .await
        .unwrap();
    assert!(
        crate::code_mode::AUDIT_RUN
            .scope(
                run,
                session.call_with_authorization(
                    operation("fixture.left"),
                    None,
                    &Authorization::Identity(identity("456", "bob"))
                )
            )
            .await
            .is_err()
    );
    assert!(receiver.try_recv().is_err());
}

#[tokio::test]
async fn control_attribution_is_captured_before_later_agent_binding() {
    let (session, mut receiver) = fixture();
    let run = crate::code_mode::AuditRun::new("return 1;", None).unwrap();
    let target = session.clone();
    let first = tokio::spawn(crate::code_mode::AUDIT_RUN.scope(run.clone(), async move {
        target
            .call_with_authorization(
                operation("fixture.before"),
                None,
                &Authorization::Identity(identity("123", "alice")),
            )
            .await
    }));
    let call = receiver.recv().await.unwrap();
    let owner =
        crate::bridge::owner("mcp", &Authorization::Identity(identity("123", "alice"))).unwrap();
    let agent = session
        .collaboration
        .dispatch(&owner, "join", json!({"name":"late worker"}))
        .await
        .unwrap();
    run.bind(
        session.client_id(),
        &owner,
        agent["agent_id"].as_str().unwrap(),
        agent["token"].as_str().unwrap(),
    )
    .unwrap();
    assert!(call.attribution.as_ref().unwrap().agent.is_none());
    call.reply.send(Ok(json!({}))).unwrap();
    first.await.unwrap().unwrap();
}
