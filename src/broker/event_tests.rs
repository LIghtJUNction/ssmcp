//! Real local queue transitions and account-scoped event audiences. These
//! fixtures create no transport, external model, or user-owned conversation.
use super::{Broker, Session};
use crate::{
    events::{Actor, Filter},
    wire::{SteeringCommand, SteeringSnapshot, SteeringState},
};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};

const OWNER: &str = "test\0github\0subject";

struct Member {
    actor: Actor,
    token: String,
}

async fn session(broker: &Broker, login: &str) -> Arc<Session> {
    let session = broker.test_register("test", login).await;
    assert_eq!(session.client_id().len(), 64);
    assert!(
        session
            .client_id()
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    );
    session
}

async fn join(session: &Session, name: &str) -> Member {
    let member = session
        .collaboration
        .dispatch(OWNER, "join", json!({"name":name}))
        .await
        .unwrap();
    Member {
        actor: Actor {
            owner: OWNER.into(),
            client_id: session.client_id().into(),
            agent_id: member["agent_id"].as_str().unwrap().into(),
        },
        token: member["token"].as_str().unwrap().into(),
    }
}

fn filter(source: &Session) -> Filter {
    Filter {
        kinds: vec!["user.input_available".into()],
        client_ids: vec![source.client_id().into()],
        ..Filter::default()
    }
}

fn subscribe(broker: &Broker, actor: &Actor, source: &Session, request_id: &str) -> String {
    broker
        .events
        .subscribe(actor, request_id, filter(source), None)
        .unwrap()["subscription_id"]
        .as_str()
        .unwrap()
        .into()
}

fn publish(
    session: &Session,
    command_id: &str,
    message_id: &str,
    target: &str,
    text: &str,
) -> SteeringSnapshot {
    session
        .steering_command(SteeringCommand::Publish {
            command_id: command_id.into(),
            message_id: message_id.into(),
            expected_revision: 0,
            text: text.into(),
            target: target.into(),
        })
        .unwrap()
}

fn unchanged_queue(session: &Session, before: &SteeringSnapshot) {
    let after = session
        .steering_command(SteeringCommand::Snapshot {})
        .unwrap();
    assert_eq!(after.revision, before.revision);
    assert_eq!(
        serde_json::to_value(&after.messages).unwrap(),
        serde_json::to_value(&before.messages).unwrap()
    );
    assert!(
        after.messages.iter().all(|message| {
            message.state == SteeringState::Queued && message.delivery.is_none()
        })
    );
}

fn assert_metadata_only(value: &Value, body: &str, tokens: &[&str]) {
    let encoded = value.to_string();
    assert!(!encoded.contains(body));
    for &token in tokens {
        assert!(!encoded.contains(token));
    }
    fn visit(value: &Value) {
        match value {
            Value::Object(fields) => {
                for (name, value) in fields {
                    assert!(!matches!(
                        name.as_str(),
                        "text" | "body" | "token" | "agent_token" | "access_token"
                    ));
                    visit(value);
                }
            }
            Value::Array(values) => values.iter().for_each(visit),
            _ => {}
        }
    }
    visit(value);
}

#[tokio::test]
async fn steering_publish_emits_one_metadata_event_without_consuming_input() {
    let broker = Broker::new("https://ssmcp.example".into());
    let source = session(&broker, "input-fixture").await;
    let member = join(&source, "observer").await;
    let subscription = subscribe(&broker, &member.actor, &source, "observe_publish");
    let message_id = "m".repeat(128);
    let body = "PRIVATE_USER_MESSAGE_SENTINEL";
    let queued = publish(
        &source,
        "publish_long_input",
        &message_id,
        &member.actor.agent_id,
        body,
    );
    let retry = publish(
        &source,
        "publish_long_input",
        &message_id,
        &member.actor.agent_id,
        body,
    );
    assert_eq!(retry.revision, queued.revision);
    let observed = broker
        .events
        .read(&member.actor, &subscription, 32)
        .unwrap();
    assert_eq!(observed["status"], "ready");
    let events = observed["events"].as_array().unwrap();
    assert_eq!(events.len(), 1);
    let event = &events[0];
    assert_eq!(event["kind"], "user.input_available");
    assert_eq!(event["client_id"], source.client_id());
    assert_eq!(event["data"]["message_id"], message_id);
    assert_eq!(event["data"]["revision"], queued.messages[0].revision);
    assert_eq!(event["data"]["target"], member.actor.agent_id);
    assert_eq!(event["data"]["queue_revision"], queued.revision);
    assert_eq!(event["data"]["observation"], "queue_changed");
    assert_metadata_only(&observed, body, &[&member.token]);
    let replay = broker
        .events
        .read(&member.actor, &subscription, 32)
        .unwrap();
    assert_eq!(replay["events"], observed["events"]);
    broker
        .events
        .ack(
            &member.actor,
            &subscription,
            observed["next_cursor"].as_u64().unwrap(),
        )
        .unwrap();
    unchanged_queue(&source, &queued);
}

#[tokio::test]
async fn queued_input_seed_is_visible_once_per_subscription() {
    let broker = Broker::new("https://ssmcp.example".into());
    let source = session(&broker, "seed-fixture").await;
    let message_id = "q".repeat(128);
    let body = "QUEUED_BEFORE_OBSERVER_SENTINEL";
    // No participant or subscription exists when the user queues this input.
    let queued = publish(&source, "publish_before_join", &message_id, "next", body);
    let member = join(&source, "late-observer").await;
    for request_id in ["first_subscription", "second_subscription"] {
        let subscription = subscribe(&broker, &member.actor, &source, request_id);
        let before = broker
            .events
            .read(&member.actor, &subscription, 32)
            .unwrap();
        assert_eq!(before["status"], "pending");
        assert!(before["events"].as_array().unwrap().is_empty());
        source.seed_user_input_events(OWNER, &member.actor.agent_id, &subscription);
        source.seed_user_input_events(OWNER, &member.actor.agent_id, &subscription);
        let observed = broker
            .events
            .read(&member.actor, &subscription, 32)
            .unwrap();
        let events = observed["events"].as_array().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["data"]["message_id"], message_id);
        assert_eq!(events[0]["data"]["revision"], queued.messages[0].revision);
        assert_eq!(events[0]["data"]["observation"], "queued_snapshot");
        assert_metadata_only(&observed, body, &[&member.token]);
    }
    unchanged_queue(&source, &queued);
}

#[tokio::test]
async fn targeted_input_availability_uses_exact_client_and_agent_audience() {
    let broker = Broker::new("https://ssmcp.example".into());
    let source = session(&broker, "private-input-fixture").await;
    let other = session(&broker, "other-client-fixture").await;
    let recipient = join(&source, "recipient").await;
    let stranger = join(&source, "stranger").await;
    let correct = subscribe(&broker, &recipient.actor, &source, "recipient_subscription");
    let unrelated = subscribe(&broker, &stranger.actor, &source, "stranger_subscription");
    // A deliberately colliding bare ID isolates the hub's client:agent audience
    // comparison. Participant authentication is performed above this hub.
    let other_client_same_id = Actor {
        client_id: other.client_id().into(),
        ..recipient.actor.clone()
    };
    let collision = subscribe(
        &broker,
        &other_client_same_id,
        &source,
        "collision_subscription",
    );
    let queued = publish(
        &source,
        "publish_private",
        "private_input",
        &recipient.actor.agent_id,
        "PRIVATE_RECIPIENT_BODY_SENTINEL",
    );
    let delivered = broker.events.read(&recipient.actor, &correct, 32).unwrap();
    assert_eq!(delivered["events"].as_array().unwrap().len(), 1);
    for (actor, subscription) in [
        (&stranger.actor, &unrelated),
        (&other_client_same_id, &collision),
    ] {
        let hidden = broker.events.read(actor, subscription, 32).unwrap();
        assert_eq!(hidden["status"], "pending");
        assert!(hidden["events"].as_array().unwrap().is_empty());
        assert!(!hidden.to_string().contains("private_input"));
    }
    assert_metadata_only(
        &delivered,
        "PRIVATE_RECIPIENT_BODY_SENTINEL",
        &[&recipient.token, &stranger.token],
    );
    unchanged_queue(&source, &queued);
}

#[tokio::test]
async fn next_input_availability_is_visible_only_to_current_session_candidates() {
    let broker = Broker::new("https://ssmcp.example".into());
    let source = session(&broker, "next-input-fixture").await;
    let other = session(&broker, "other-next-client-fixture").await;
    let first = join(&source, "first-candidate").await;
    let second = join(&source, "second-candidate").await;
    let departed = join(&source, "departed-candidate").await;
    let foreign = join(&other, "other-client-candidate").await;
    let first_sub = subscribe(&broker, &first.actor, &source, "first_candidate_sub");
    let second_sub = subscribe(&broker, &second.actor, &source, "second_candidate_sub");
    let departed_sub = subscribe(&broker, &departed.actor, &source, "departed_candidate_sub");
    let foreign_sub = subscribe(&broker, &foreign.actor, &source, "foreign_candidate_sub");
    source
        .collaboration
        .dispatch(
            OWNER,
            "leave",
            json!({"agent_id":departed.actor.agent_id,"token":departed.token}),
        )
        .await
        .unwrap();
    let queued = publish(
        &source,
        "publish_next",
        "next_input",
        "next",
        "NEXT_BODY_SENTINEL",
    );
    for (actor, subscription) in [(&first.actor, &first_sub), (&second.actor, &second_sub)] {
        let visible = broker.events.read(actor, subscription, 32).unwrap();
        let events = visible["events"].as_array().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["data"]["target"], "next");
    }
    for (actor, subscription) in [
        (&departed.actor, &departed_sub),
        (&foreign.actor, &foreign_sub),
    ] {
        let hidden = broker.events.read(actor, subscription, 32).unwrap();
        assert_eq!(hidden["status"], "pending");
        assert!(hidden["events"].as_array().unwrap().is_empty());
    }
    unchanged_queue(&source, &queued);
}

#[tokio::test]
async fn closing_source_invalidates_existing_observations_without_closing_other_sources() {
    let broker = Broker::new("https://ssmcp.example".into());
    let source = session(&broker, "closing-source-fixture").await;
    let other = session(&broker, "retained-source-fixture").await;
    let local = join(&source, "local-observer").await;
    let observer = join(&other, "cross-client-observer").await;
    let local_sub = subscribe(&broker, &local.actor, &source, "local_closing_sub");
    let selected_sub = subscribe(&broker, &observer.actor, &source, "selected_closing_sub");
    let retained_sub = subscribe(&broker, &observer.actor, &other, "retained_sub");
    let mut waiting = Box::pin(broker.events.wait(&observer.actor, &selected_sub, 32, 5000));
    tokio::select! {
        biased;
        value = &mut waiting => panic!("unexpected event result before close: {value:?}"),
        _ = tokio::task::yield_now() => {},
    }
    broker.events.close_client(source.client_id());
    let error = tokio::time::timeout(Duration::from_secs(1), waiting)
        .await
        .unwrap()
        .unwrap_err();
    assert!(error.starts_with("events_client_closed:"));
    for (actor, subscription) in [(&local.actor, &local_sub), (&observer.actor, &selected_sub)] {
        let error = broker.events.read(actor, subscription, 32).unwrap_err();
        assert!(error.starts_with("events_client_closed:"));
    }
    let retained = broker
        .events
        .read(&observer.actor, &retained_sub, 32)
        .unwrap();
    assert_eq!(retained["status"], "pending");
    let retry = broker
        .events
        .subscribe(
            &observer.actor,
            "selected_closing_sub",
            filter(&source),
            None,
        )
        .unwrap();
    assert_eq!(retry["subscription_id"], selected_sub);
    assert_eq!(retry["status"], "closed");
    assert_eq!(retry["duplicate"], true);
}

#[tokio::test]
async fn disconnect_all_invalidates_queued_input_event_subscriptions() {
    let broker = Broker::new("https://ssmcp.example".into());
    let source = session(&broker, "disconnect-fixture").await;
    let member = join(&source, "disconnect-observer").await;
    let subscription = subscribe(&broker, &member.actor, &source, "disconnect_sub");
    publish(
        &source,
        "publish_before_close",
        "input_before_close",
        &member.actor.agent_id,
        "CLOSED_BODY_SENTINEL",
    );
    broker.disconnect_all().await;
    assert!(!source.is_alive());
    assert!(
        source
            .steering_command(SteeringCommand::Snapshot {})
            .is_err()
    );
    source.seed_user_input_events(OWNER, &member.actor.agent_id, &subscription);
    let error = broker
        .events
        .read(&member.actor, &subscription, 32)
        .unwrap_err();
    assert!(error.starts_with("events_client_closed:"));
}
