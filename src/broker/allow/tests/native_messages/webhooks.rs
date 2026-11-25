use super::*;

#[tokio::test]
async fn external_notifications_require_account_and_target_and_deduplicate_queue_retries() {
    let fixture = Fixture::new().await;
    let (mut socket, target) = connected(&fixture, Some("42"), true, vec![]).await;
    let client = crate::http_client::builder().no_proxy().build().unwrap();
    let url = format!("{}/api/client/webhooks", fixture.base);
    let recipient = destination(&target);
    let post = |at: u64| {
        client
            .post(&url)
            .query(&[("recipient_id", &recipient)])
            .json(
                &json!({"schema_version":1,"id":"run-7","event":"build.done",
                "text":"checks passed","data":{"count":3},"created_at_ms":at}),
            )
    };
    let own = oauth(&fixture, IDENTITY_RESOURCE, "42");
    let other = oauth(&fixture, IDENTITY_RESOURCE, "7");
    assert_eq!(post(12).send().await.unwrap().status(), 401);
    assert!(
        !post(12)
            .bearer_auth(other)
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    let mut queued = None;
    for (supported, when) in [(false, 11), (true, 12), (true, 18)] {
        let request = post(when).bearer_auth(&own).send();
        let reply = async {
            let Frame::Call {
                id,
                operation: Operation::CallTool { name, .. },
                ..
            } = receive(&mut socket).await
            else {
                panic!("authenticated client capability probe required");
            };
            assert_eq!(name, "ssmcp.client.capabilities");
            send(
                &mut socket,
                Frame::Reply {
                    id,
                    result: Ok(crate::backend::result(
                        json!({"webhook_notifications":u32::from(supported)}),
                    )),
                },
            )
            .await;
        };
        let (response, ()) = tokio::join!(request, reply);
        let response = response.unwrap();
        if !supported {
            assert_eq!(response.status(), 409);
            continue;
        }
        assert_eq!(response.status(), 202);
        let receipt: messages::Receipt = response.json().await.unwrap();
        assert_eq!(receipt.status, Status::Queued);
        assert_eq!(receipt.duplicate, queued.is_some());
        if let Some(prior) = &queued {
            assert_eq!(prior, &receipt.message_id);
        }
        queued = Some(receipt.message_id);
    }
    let inbox = local(&mut socket, 1, Command::Inbox {}).await.unwrap();
    assert_eq!(inbox.messages.len(), 1);
    let message = &inbox.messages[0];
    assert_eq!(message.sender.kind, Origin::Webhook);
    assert_eq!(message.sender.account.subject, "42");
    assert_eq!(message.recipient.id, recipient);
    assert_eq!(
        serde_json::from_str::<Value>(&message.text).unwrap(),
        json!({"event":"build.done","text":"checks passed","data":{"count":3}})
    );
}
