use super::{queue::Inbox, *};
use serde_json::json;
use std::{sync::Arc, time::Duration};

fn message(inbox: &Inbox, id: u32) -> PeerMessage {
    PeerMessage {
        message_id: format!("peer_{id:032x}"),
        delivery_id: format!("delivery_{id:064x}"),
        source: Source {
            principal: Principal::Account {
                account: ControlAccount {
                    provider: "github".into(),
                    subject: "1".into(),
                    login: "fixture".into(),
                },
            },
            run_id: Some("a".repeat(32)),
            participant: None,
            controller: None,
        },
        target: Target {
            client_id: "a".repeat(64),
            session_id: inbox.session_id().into(),
            thread_id: None,
        },
        text: "peer text · 中文 e\u{301}".into(),
        topic: None,
        reply_to: None,
        sent_at_ms: now_ms(),
        ttl_ms: DEFAULT_TTL_MS,
    }
}

#[tokio::test]
async fn native_process_and_runtime_generations_are_distinct_and_read_is_non_consuming() {
    let first = Inbox::new().unwrap();
    let second = Inbox::new().unwrap();
    assert_eq!(
        first.capability()["process_id"],
        second.capability()["process_id"]
    );
    assert_eq!(first.capability()["pid"], std::process::id());
    assert_ne!(first.session_id(), second.session_id());
    let msg = message(&first, 1);
    let target = msg.target.clone();
    first.deliver(msg.clone()).unwrap();
    assert!(second.deliver(msg).unwrap_err().contains("generation"));
    let one = first.read(&target, 0, 32).unwrap();
    let two = first.read(&target, 0, 32).unwrap();
    assert_eq!(one, two);
    assert_eq!(one["messages"].as_array().unwrap().len(), 1);
    assert_eq!(one["consumed"], false);
    assert_eq!(first.snapshot().candidates.len(), 1);
    first.close();
    second.close();
}

#[tokio::test]
async fn duplicate_delivery_and_ack_never_reenqueue_changed_or_handled_content() {
    let inbox = Inbox::new().unwrap();
    let msg = message(&inbox, 2);
    let target = msg.target.clone();
    assert_eq!(inbox.deliver(msg.clone()).unwrap()["duplicate"], false);
    assert_eq!(inbox.deliver(msg.clone()).unwrap()["duplicate"], true);
    let mut changed = msg.clone();
    changed.text.push('!');
    assert!(
        inbox
            .deliver(changed)
            .unwrap_err()
            .contains("different content")
    );
    inbox
        .ack(&target, std::slice::from_ref(&msg.delivery_id), None)
        .unwrap();
    assert!(inbox.snapshot().candidates.is_empty());
    assert_eq!(inbox.deliver(msg.clone()).unwrap()["state"], "acknowledged");
    assert!(inbox.snapshot().candidates.is_empty());
    assert!(
        inbox
            .ack(&target, std::slice::from_ref(&msg.delivery_id), None)
            .unwrap()["acknowledged"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    inbox.close();
}

#[tokio::test]
async fn mailbox_filters_and_atomic_ack_do_not_consume_another_participant() {
    let inbox = Inbox::new().unwrap();
    let a = message(&inbox, 3);
    let mut b = message(&inbox, 4);
    b.target.thread_id = Some(format!("agent_{}", "b".repeat(32)));
    inbox.deliver(a.clone()).unwrap();
    inbox.deliver(b.clone()).unwrap();
    let page = inbox.read(&a.target, 0, 32).unwrap();
    assert_eq!(page["messages"].as_array().unwrap().len(), 1);
    assert!(
        inbox
            .ack(
                &a.target,
                &[a.delivery_id.clone(), b.delivery_id.clone()],
                None
            )
            .is_err()
    );
    assert_eq!(inbox.snapshot().candidates.len(), 2);
    inbox.ack(&a.target, &[], Some(2)).unwrap();
    assert_eq!(inbox.snapshot().candidates.len(), 1);
    assert_eq!(
        inbox.read(&b.target, 0, 32).unwrap()["messages"][0]["message"]["delivery_id"],
        b.delivery_id
    );
    assert!(inbox.ack(&b.target, &[], Some(3)).is_err());
    inbox.close();
}

#[tokio::test]
async fn cancelled_wait_is_non_consuming_and_close_wakes_waiters() {
    let inbox = Inbox::new().unwrap();
    let msg = message(&inbox, 5);
    let target = msg.target.clone();
    let waiting = inbox.clone();
    let wait_target = target.clone();
    let task = tokio::spawn(async move { waiting.wait(&wait_target, 0, 32, 5000).await });
    tokio::task::yield_now().await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    inbox.deliver(msg.clone()).unwrap();
    assert_eq!(
        inbox.wait(&target, 0, 32, 0).await.unwrap()["messages"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    inbox.ack(&target, &[msg.delivery_id], None).unwrap();
    let waiting = inbox.clone();
    let task = tokio::spawn(async move { waiting.wait(&target, 1, 32, 5000).await });
    tokio::task::yield_now().await;
    inbox.close();
    assert!(
        tokio::time::timeout(Duration::from_millis(250), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err()
            .contains("closed")
    );
    let weak = Arc::downgrade(&inbox);
    drop(inbox);
    tokio::task::yield_now().await;
    assert!(weak.upgrade().is_none());
}

#[tokio::test]
async fn expiry_reclaims_payload_without_a_reader_or_periodic_poll() {
    let inbox = Inbox::new().unwrap();
    let mut msg = message(&inbox, 6);
    msg.ttl_ms = 15;
    inbox.deliver(msg.clone()).unwrap();
    let revision = inbox.revision();
    tokio::time::timeout(Duration::from_millis(500), async {
        while inbox.revision() == revision {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let snapshot = inbox.snapshot();
    assert!(snapshot.candidates.is_empty());
    assert_eq!(snapshot.retained_bytes, 0);
    assert_eq!(
        inbox.status(&msg.target, &[msg.delivery_id]).unwrap()["deliveries"][0]["state"],
        "expired"
    );
    inbox.close();
}

#[tokio::test]
async fn byte_and_page_budgets_are_enforced_before_acceptance() {
    let inbox = Inbox::new().unwrap();
    let mut accepted = 0;
    for id in 10..150 {
        let mut msg = message(&inbox, id);
        msg.text = "x".repeat(MAX_TEXT_BYTES);
        match inbox.deliver(msg) {
            Ok(_) => accepted += 1,
            Err(e) => {
                assert!(e.contains("full"));
                break;
            }
        }
    }
    assert!(accepted > 1 && accepted < MAX_MESSAGES);
    assert!(inbox.snapshot().retained_bytes <= MAX_INBOX_BYTES);
    let target = message(&inbox, 10).target;
    let page = inbox.read(&target, 0, 32).unwrap();
    assert!(page["messages"].as_array().unwrap().len() < accepted);
    assert_eq!(page["has_more"], true);
    let mut oversized = message(&inbox, 999);
    oversized.text = "x".repeat(MAX_TEXT_BYTES + 1);
    assert!(inbox.deliver(oversized).unwrap_err().contains("UTF-8"));
    inbox.close();
}

#[tokio::test]
async fn active_candidate_dedup_survives_retired_receipt_pressure() {
    let inbox = Inbox::new().unwrap();
    let oldest = message(&inbox, 1000);
    inbox.deliver(oldest.clone()).unwrap();
    for id in 1001..1300 {
        let msg = message(&inbox, id);
        inbox.deliver(msg.clone()).unwrap();
        inbox.ack(&msg.target, &[msg.delivery_id], None).unwrap();
    }
    assert_eq!(inbox.deliver(oldest.clone()).unwrap()["duplicate"], true);
    assert_eq!(inbox.snapshot().candidates.len(), 1);
    inbox
        .ack(&oldest.target, &[oldest.delivery_id], None)
        .unwrap();
    assert!(inbox.snapshot().candidates.is_empty());
    inbox.close();
}

#[test]
fn typed_envelopes_cannot_supply_user_roles_or_private_agent_tokens() {
    let inbox = Inbox::new().unwrap();
    let mut value = serde_json::to_value(message(&inbox, 99)).unwrap();
    value["role"] = json!("system");
    assert!(serde_json::from_value::<PeerMessage>(value).is_err());
    let mut value = serde_json::to_value(message(&inbox, 99)).unwrap();
    value["source"]["token"] = json!("secret");
    assert!(serde_json::from_value::<PeerMessage>(value).is_err());
    let mut msg = message(&inbox, 99);
    msg.text = "\u{1b}[2J".into();
    assert!(msg.validate().is_err());
}

#[tokio::test]
async fn encoded_pages_include_envelope_bytes_in_the_public_read_budget() {
    for text_bytes in (1024..=2048).step_by(16) {
        let inbox = Inbox::new().unwrap();
        let mut target = None;
        for id in 0..MAX_READ_MESSAGES {
            let mut msg = message(&inbox, 2000 + id as u32);
            msg.text = "x".repeat(text_bytes);
            target = Some(msg.target.clone());
            inbox.deliver(msg).unwrap();
        }
        let page = inbox.read(&target.unwrap(), 0, MAX_READ_MESSAGES).unwrap();
        assert!(
            serde_json::to_vec(&page).unwrap().len() <= MAX_READ_BYTES,
            "page overflow at text length {text_bytes}"
        );
        inbox.close();
    }
}
