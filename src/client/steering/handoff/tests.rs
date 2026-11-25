use super::*;
use crate::{
    runtime::call_scope::CallScope,
    wire::{CodeContext, ControlAccount, ControlAgent, ControlAttribution},
};

#[cfg(target_os = "linux")]
use crate::collab::Hub;

#[cfg(target_os = "linux")]
struct Identity {
    id: String,
    token: String,
    subject: String,
}

#[cfg(target_os = "linux")]
impl Identity {
    async fn join(hub: &Hub, subject: &str, role: &str) -> Self {
        let joined = hub
            .dispatch(subject, "join", json!({"name":"same display","role":role}))
            .await
            .unwrap();
        Self {
            id: joined["agent_id"].as_str().unwrap().into(),
            token: joined["token"].as_str().unwrap().into(),
            subject: subject.into(),
        }
    }

    async fn call(
        &self,
        hub: &Hub,
        m: &mut Manager,
        room: &str,
        request: Value,
        now: Instant,
    ) -> Result<Value, String> {
        let bound = hub.bind(&self.subject, &self.id, &self.token)?;
        call_from_wire(
            m,
            room,
            &self.subject,
            ControlAgent {
                id: bound.agent_id,
                name: bound.name,
            },
            request,
            now,
        )
        .await
    }
}

async fn call_from_wire(
    manager: &mut Manager,
    room: &str,
    subject: &str,
    agent: ControlAgent,
    request: Value,
    now: Instant,
) -> Result<Value, String> {
    let context = CodeContext {
        run_id: "a".repeat(32),
        code: "fixture".into(),
        timestamp_ms: 1,
        session_id: Some(room.into()),
        thread_id: Some(agent.id.clone()),
    };
    let attribution = ControlAttribution {
        account: ControlAccount {
            provider: "github".into(),
            subject: subject.into(),
            login: "fixture".into(),
        },
        owner: None,
        agent: Some(agent),
    };
    let scope =
        CallScope::from_relay_for_client(7, Some(room), 1, Some(&attribution), Some(&context))?;
    scope
        .scope(async { manager.handoff_member(&Source::capture(), request, now) })
        .await
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn authenticated_solo_handoff_rejects_other_accounts_rooms_expiry_and_replay() {
    let room = "b".repeat(64);
    let hub = Hub::for_session(&room);
    let old = Identity::join(&hub, "123", "main").await;
    let next = Identity::join(&hub, "123", "main").await;
    let other = Identity::join(&hub, "456", "main").await;
    let mut m = Manager::default();
    let _transport = m.connect_with_chat(&room, 7, true);
    m.merge(&SteeringSnapshot {
        schema_version: 1,
        client_id: room.clone(),
        revision: 1,
        agents: hub.presence().unwrap(),
        messages: vec![],
    })
    .unwrap();
    let now = Instant::now();
    for member in [&old, &next, &other] {
        member
            .call(&hub, &mut m, &room, json!({}), now)
            .await
            .unwrap();
    }
    let original = m.submit("old private task", Some(&old.id)).unwrap();
    let foreign = m.submit("foreign task", Some(&other.id)).unwrap();
    let selected = format!("{}>{}", old.id, next.id);
    assert!(
        m.offer_control_handoff(
            &room,
            "/fixture",
            &format!("{}>{}", old.id, other.id),
            "pending",
            Some("123"),
            now
        )
        .is_err()
    );
    let key = m
        .offer_control_handoff(
            &room,
            "/fixture",
            &selected,
            "done A; pending B; unknown C",
            Some("123"),
            now,
        )
        .unwrap();
    let req = json!({"handoff_id":key});
    assert!(
        m.handoff_member(&Source::default(), req.clone(), now)
            .is_err()
    );
    assert!(
        old.call(&hub, &mut m, &room, req.clone(), now)
            .await
            .is_err()
    );
    assert!(
        next.call(&hub, &mut m, &"c".repeat(64), req.clone(), now)
            .await
            .is_err()
    );
    assert!(
        other
            .call(&hub, &mut m, &room, req.clone(), now)
            .await
            .is_err()
    );
    assert!(
        next.call(
            &hub,
            &mut m,
            &room,
            json!({"handoff_id":key,"subject":"123"}),
            now
        )
        .await
        .is_err()
    );
    assert!(
        next.call(&hub, &mut m, &room, req.clone(), now + TTL)
            .await
            .is_err()
    );
    let key = m
        .offer_control_handoff(
            &room,
            "/fixture",
            &selected,
            "A done, B pending, C unknown",
            Some("123"),
            now + TTL,
        )
        .unwrap();
    assert!(
        next.call(&hub, &mut m, &room, req, now + TTL)
            .await
            .is_err()
    );
    let req = json!({"handoff_id":key});
    let ack = next
        .call(&hub, &mut m, &room, req.clone(), now + TTL)
        .await
        .unwrap();
    let data = &ack["handoff"];
    assert_eq!(data["workspace"], "/fixture");
    assert_eq!(data["control"], room);
    assert_eq!(data["predecessor"], old.id);
    assert_eq!(data["successor"], next.id);
    assert_eq!(data["request_refs"][0]["id"], original);
    assert!(!data.to_string().contains(&foreign));
    assert!(!data.to_string().contains("private task"));
    assert!(!data.to_string().contains(&old.token));
    assert_eq!(ack["duplicate"], false);
    let retry = next
        .call(&hub, &mut m, &room, req.clone(), now + TTL)
        .await
        .unwrap();
    assert_eq!(retry["duplicate"], true);
    assert_eq!(retry["handoff"], *data);
    assert_eq!(m.outbox.records.len(), 2);
    m.submit("new operator input", None).unwrap();
    assert_eq!(m.outbox.records.last().unwrap().message.target, next.id);
    assert_eq!(m.outbox.records[0].message.target, old.id);
    hub.expire_lease_for_test("123", &next.id);
    assert!(
        next.call(&hub, &mut m, &room, req.clone(), now + TTL)
            .await
            .is_err()
    );
    m.disconnect();
    assert!(m.handoffs.pending.is_none() && m.handoffs.accepted.is_none());
}

#[tokio::test]
async fn first_solo_handoff_from_unbound_main_requires_the_original_account() {
    let room = "d".repeat(64);
    let next = ControlAgent {
        id: format!("agent_{}", "a".repeat(32)),
        name: "main fixture".into(),
    };
    let mut m = Manager::default();
    let _transport = m.connect_with_chat(&room, 7, true);
    let roster: SteeringSnapshot = serde_json::from_value(json!({
        "schema_version": 1, "client_id": room, "revision": 1, "messages": [],
        "agents": [{"agent_id": next.id, "name": next.name, "role":"main", "status":"idle"}]
    }))
    .unwrap();
    m.merge(&roster).unwrap();
    let now = Instant::now();
    call_from_wire(&mut m, &room, "123", next.clone(), json!({}), now)
        .await
        .unwrap();
    m.submit("before the handoff", None).unwrap();
    for subject in [None, Some("456")] {
        assert!(
            m.offer_control_handoff(&room, "/fixture", &next.id, "unknown job ref", subject, now)
                .is_err()
        );
    }
    let key = m
        .offer_control_handoff(
            &room,
            "/fixture",
            &next.id,
            "previous job unknown #42",
            Some("123"),
            now,
        )
        .unwrap();
    let request = json!({"handoff_id":key});
    assert!(
        m.handoff_member(&Source::default(), request.clone(), now)
            .is_err()
    );
    assert!(
        call_from_wire(
            &mut m,
            &"c".repeat(64),
            "123",
            next.clone(),
            request.clone(),
            now
        )
        .await
        .is_err()
    );
    assert!(
        call_from_wire(&mut m, &room, "456", next.clone(), request.clone(), now)
            .await
            .is_err()
    );
    assert!(m.handoffs.coordinator().is_none());
    let ack = call_from_wire(&mut m, &room, "123", next.clone(), request.clone(), now)
        .await
        .unwrap();
    assert_eq!(ack["duplicate"], false);
    assert_eq!(ack["handoff"]["predecessor"], "main");
    assert_eq!(ack["handoff"]["successor"], next.id);
    assert_eq!(ack["handoff"]["note"], "previous job unknown #42");
    let retry = call_from_wire(&mut m, &room, "123", next.clone(), request, now)
        .await
        .unwrap();
    assert_eq!(retry["duplicate"], true);
    assert_eq!(retry["handoff"], ack["handoff"]);
    m.submit("after the handoff", None).unwrap();
    assert_eq!(m.outbox.records.len(), 2);
    assert_eq!(m.outbox.records[0].message.target, "next");
    assert_eq!(m.outbox.records[1].message.target, next.id);
}
