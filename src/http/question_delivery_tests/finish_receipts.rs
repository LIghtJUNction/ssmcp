//! Completion receipts cross the authenticated client/server boundary.
//! Keep the local queue out of these mixed-version regression cases.
use super::*;

const REASON: &str = "The requested work and its acceptance checks are complete.";

fn set_finish(fixture: &Fixture, actor: Option<Value>, status: &str, approved: bool) {
    let decision = actor.as_ref().map(
        |actor| json!({"choice":"yes","comment":null,"actor":actor,"revision":1,"decided_at_ms":2}),
    );
    let mut state = fixture.native.lock().unwrap();
    state.view = json!({
        "id":"retained_question","title":"Request to finish","question":REASON,
        "options":["No","Yes"],"state":if decision.is_some() {"answered"} else {"pending"},
        "answer":decision.as_ref().map(|_|"Yes"),"created_at_ms":1,
        "answered_at_ms":decision.as_ref().map(|_|2),"kind":"finish",
        "requester":{"provider":"github","subject":"42",
            "thread_id":fixture.agent_id,"name":"asking agent"},
        "finish":{"reason":REASON,"default_decision":"no","decision":decision,
            "status":status,"approved":approved,"invalidation_reason":null},
        "answer_actor":actor,"answer_revision":if decision.is_some() {1} else {0},
        "automatic_delivery":true,
        "delivery":if decision.is_some() {"delivered"} else {"not_answered"}
    });
    // The fixture models a retained receipt. It has no separate answer batch
    // to deliver while the public completion request is being observed.
    state.pending = false;
}

async fn request(fixture: &Fixture) -> Result<Value, String> {
    let arguments = finish_request::Arguments::parse(json!({
        "client_id":fixture.session.client_id(),"reason":REASON,
        "request_id":"retained_question","timeout_ms":2000,
        "agent":{"id":fixture.agent_id,"token":fixture.agent_token}
    }))
    .unwrap();
    timeout(
        Duration::from_secs(3),
        finish_request::execute(
            fixture.broker.clone(),
            "mcp".into(),
            fixture.token.clone(),
            arguments,
        ),
    )
    .await
    .expect("authenticated finish request should reach the native peer")
}

fn assert_requests(fixture: &Fixture, count: usize) {
    let state = fixture.native.lock().unwrap();
    let requests: Vec<_> = state
        .finish_operations
        .iter()
        .filter(|op| op["op"] == "request")
        .collect();
    assert_eq!(requests.len(), count);
    for operation in requests {
        assert_eq!(operation["request_id"], "retained_question");
        assert_eq!(operation["reason"], REASON);
    }
    assert!(
        !fixture.peer.is_finished(),
        "authenticated peer must remain healthy"
    );
}

#[tokio::test]
async fn request_finish_rejects_legacy_agent_approval_from_authenticated_native_peer() {
    let fixture = Fixture::new().await;
    let peer = fixture
        .session
        .collaboration
        .dispatch(&fixture.owner, "join", json!({"name":"legacy approver"}))
        .await
        .unwrap();
    assert_ne!(peer["agent_id"], fixture.agent_id);
    set_finish(
        &fixture,
        Some(json!({"origin":"agent","agent":{
            "provider":"github","subject":"42",
            "thread_id":peer["agent_id"],"name":"legacy approver"
        }})),
        "approved",
        true,
    );

    let error = request(&fixture).await.unwrap_err();
    assert_eq!(
        error,
        "finish_human_approval_required: native approval must contain the local user's recorded Yes; upgrade an older client"
    );
    assert_requests(&fixture, 1);
    timeout(Duration::from_secs(3), async {
        loop {
            let cancelled = fixture
                .native
                .lock()
                .unwrap()
                .finish_operations
                .iter()
                .any(|op| op["op"] == "cancel" && op["request_id"] == "retained_question");
            if cancelled {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the existing failure guard must cancel only the original request");
    assert!(!fixture.peer.is_finished());
    assert!(fixture.native.lock().unwrap().acknowledgements.is_empty());
}

#[tokio::test]
async fn request_finish_accepts_a_consistent_human_receipt_from_authenticated_native_peer() {
    let fixture = Fixture::new().await;
    set_finish(&fixture, Some(json!({"origin":"human"})), "approved", true);
    let result = request(&fixture).await.unwrap();
    let data = &result["structuredContent"];
    assert_eq!(data["request_id"], "retained_question");
    assert_eq!(data["approved"], true);
    assert_eq!(data["decision"]["actor"]["origin"], "human");
    assert_eq!(data["decision"]["choice"], "yes");
    assert_eq!(data["request"]["finish"]["status"], "approved");
    assert_eq!(data["decision"], data["request"]["finish"]["decision"]);
    assert_requests(&fixture, 1);
    assert_eq!(fixture.native.lock().unwrap().finish_operations.len(), 1);
}

#[tokio::test]
async fn request_finish_keeps_pending_timeouts_unapproved_when_resuming_the_same_id() {
    let fixture = Fixture::new().await;
    set_finish(&fixture, None, "pending", false);
    for _ in 0..2 {
        let result = request(&fixture).await.unwrap();
        let data = &result["structuredContent"];
        assert_eq!(data["request_id"], "retained_question");
        assert_eq!(data["timed_out"], true);
        assert_eq!(data["default_decision"], "no");
        assert_eq!(data["decision"], Value::Null);
        assert_eq!(data["approved"], false);
        assert_eq!(data["request"]["finish"]["status"], "pending");
    }
    assert_requests(&fixture, 2);
    assert_eq!(fixture.native.lock().unwrap().finish_operations.len(), 2);
}
