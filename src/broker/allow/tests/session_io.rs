//! Real loopback account HTTP and registered WebSocket sessions, without models.
use super::*;

#[tokio::test]
async fn account_chat_directory_send_and_poll_use_authenticated_exact_targets() {
    let fixture = Fixture::new().await;
    let token = fixture.token("42").await;
    let (mut socket, session) = fixture.ready(Some(&token), Vec::new()).await;
    let actor = identity("42", "octocat");
    let owner =
        crate::bridge::owner(IDENTITY_RESOURCE, &Authorization::Identity(actor.clone())).unwrap();
    let member = session
        .collaboration
        .dispatch(&owner, "join", json!({"name":"same display name"}))
        .await
        .unwrap();
    let thread = member["agent_id"].as_str().unwrap();
    let http = crate::http_client::builder().no_proxy().build().unwrap();
    let directory: Value = http
        .get(format!("{}/api/client/sessions", fixture.base))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let process = directory["clients"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["id"] == session.client_id())
        .unwrap();
    assert_eq!(process["kind"], "process");
    assert!(
        process["threads"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["thread_id"].is_null())
    );
    assert!(
        process["threads"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["thread_id"] == thread)
    );
    let request = json!({"action":"send","request_id":"io_http","text":"real account input 中文",
        "recipients":[{"client_id":session.client_id(),"thread_id":null},{"client_id":session.client_id(),"thread_id":thread}]});
    assert_eq!(
        process["reply_native_supported"], false,
        "an unprobed first-use client is not advertised by label/version"
    );
    let post = http
        .post(format!("{}/api/client/io", fixture.base))
        .bearer_auth(&token)
        .json(&request)
        .send();
    let native = async {
        let Frame::Call { id, operation, .. } = receive(&mut socket).await else {
            panic!("native catalog discovery required");
        };
        assert!(matches!(operation, Operation::ListTools { .. }));
        send(&mut socket,Frame::Reply{id,result:Ok(json!({"tools":[{"name":"ssmcp.ui.reply","inputSchema":{"type":"object"}},
            {"name":"ssmcp.ui.history","inputSchema":{"oneOf":[{"properties":{"action":{"const":"thread"}}}]}}]}))}).await;
    };
    let (response, ()) = timeout(Duration::from_secs(5), async { tokio::join!(post, native) })
        .await
        .unwrap();
    let sent: Value = response
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(session.io_reply_advertised());
    assert!(session.thread_history_advertised());
    assert_eq!(sent["account"]["subject"], "42");
    assert_eq!(sent["recipients"].as_array().unwrap().len(), 2);
    let replay: Value = http
        .post(format!("{}/api/client/io", fixture.base))
        .bearer_auth(&token)
        .json(&request)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(sent["recipients"], replay["recipients"]);
    assert_eq!(replay["duplicate"], true);
    let mut changed = request.clone();
    changed["recipients"].as_array_mut().unwrap().pop();
    assert_eq!(
        http.post(format!("{}/api/client/io", fixture.base))
            .bearer_auth(&token)
            .json(&changed)
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::CONFLICT
    );
    let mut forged = request;
    forged["sender_kind"] = json!("local_user");
    assert_eq!(
        http.post(format!("{}/api/client/io", fixture.base))
            .bearer_auth(&token)
            .json(&forged)
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::BAD_REQUEST
    );
    let main =
        fixture
            .broker
            .session_io
            .receive(&fixture.broker, &session, &actor, None, 32, 100_000);
    assert_eq!(main.len(), 1);
    assert!(main[0]["thread_id"].is_null());
    let progress = json!({"request_id":"io_http","input_id":main[0]["input_id"],"output_id":format!("output_{}","1".repeat(32)),"kind":"progress","state":"running","text":"working"});
    fixture
        .broker
        .session_io
        .reply(&fixture.broker, &session, &actor, None, progress)
        .unwrap();
    let poll = json!({"action":"poll","request_id":"io_http","after":0,"limit":64,"wait_ms":0});
    let page: Value = http
        .post(format!("{}/api/client/io", fixture.base))
        .bearer_auth(&token)
        .json(&poll)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(page["outputs"][0]["kind"], "progress");
    assert!(
        page["recipients"]
            .as_array()
            .unwrap()
            .iter()
            .all(|v| v["state"] != "done")
    );
    let done = json!({"request_id":"io_http","input_id":main[0]["input_id"],"output_id":format!("output_{}","2".repeat(32)),"kind":"result","state":"done","text":"actual answer 中文"});
    fixture
        .broker
        .session_io
        .reply(&fixture.broker, &session, &actor, None, done)
        .unwrap();
    let page: Value = http
        .post(format!("{}/api/client/io", fixture.base))
        .bearer_auth(&token)
        .json(&poll)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(page["next_after"], 2);
    assert_eq!(page["outputs"][1]["text"], "actual answer 中文");
    assert!(
        page["recipients"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["thread_id"] == thread && v["state"] == "queued")
    );
    let other = fixture.token("7").await;
    assert!(
        !http
            .post(format!("{}/api/client/io", fixture.base))
            .bearer_auth(other)
            .json(&poll)
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    let outputs = json!({"action":"outputs","client_id":session.client_id(),"main":true,"cursor":0,"limit":64});
    let page: Value = http
        .post(format!("{}/api/client/io", fixture.base))
        .bearer_auth(&token)
        .json(&outputs)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(page["outputs"].as_array().unwrap().len(), 2);
    assert_eq!(page["scope"]["kind"], "main");
    close(socket).await;
}

#[tokio::test]
async fn account_chat_old_native_catalog_cannot_enable_or_enqueue_by_display_name() {
    let fixture = Fixture::new().await;
    let token = fixture.token("42").await;
    let (mut socket, session) = fixture.ready(Some(&token), Vec::new()).await;
    let request = json!({"action":"send","request_id":"io_old","text":"must remain unqueued","recipients":[{"client_id":session.client_id(),"thread_id":null}]});
    let http = crate::http_client::builder().no_proxy().build().unwrap();
    let post = http
        .post(format!("{}/api/client/io", fixture.base))
        .bearer_auth(&token)
        .json(&request)
        .send();
    let native = async {
        let Frame::Call { id, operation, .. } = receive(&mut socket).await else {
            panic!("native catalog discovery required");
        };
        assert!(matches!(operation, Operation::ListTools { .. }));
        send(&mut socket,Frame::Reply{id,result:Ok(json!({"tools":[{"name":"terminal.read"}],"display_name":"ssmcp.ui.reply","reply_native_supported":true}))}).await;
    };
    let (response, ()) = timeout(Duration::from_secs(5), async { tokio::join!(post, native) })
        .await
        .unwrap();
    let response = response.unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::CONFLICT);
    assert_eq!(
        response.json::<Value>().await.unwrap()["error"],
        "io_reply_unsupported"
    );
    assert!(!session.io_reply_advertised());
    assert!(
        fixture
            .broker
            .session_io
            .poll(&fixture.broker, &identity("42", "octocat"), "io_old", 0, 64)
            .is_err()
    );
    close(socket).await;
}
