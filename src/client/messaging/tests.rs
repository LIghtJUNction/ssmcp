use super::*;
use crate::wire::{
    ControlAccount,
    messages::{RecipientKind, Status},
};

fn recipient(digit: &str) -> Recipient {
    let client_id = digit.repeat(64);
    Recipient {
        id: format!("client:{client_id}"),
        client_id,
        agent_id: None,
        display_name: "same visible name".into(),
        kind: RecipientKind::AccountInbox,
        account: ControlAccount {
            provider: "github".into(),
            subject: "42".into(),
            login: "alice".into(),
        },
        online: true,
        sendable: true,
    }
}
fn receipt(recipient: &Recipient, request: &str, status: Status) -> Receipt {
    Receipt {
        request_id: request.into(),
        message_id: format!("native_{}", "a".repeat(32)),
        recipient_id: recipient.id.clone(),
        status,
        created_at_ms: 1,
        updated_at_ms: 2,
        included_at_ms: matches!(status, Status::Included | Status::Acknowledged).then_some(2),
        acknowledged_at_ms: (status == Status::Acknowledged).then_some(2),
        duplicate: false,
    }
}
fn ready() -> (Manager, mpsc::Receiver<Request>, Recipient, Recipient) {
    let mut manager = Manager::default();
    let mut receiver = manager.connect(true);
    let list = receiver.try_recv().unwrap();
    assert!(matches!(list.command, Command::List {}));
    let one = recipient("1");
    let two = recipient("2");
    manager.reply(
        list.id,
        Ok(Reply {
            recipients: vec![one.clone(), two.clone()],
            ..Default::default()
        }),
    );
    (manager, receiver, one, two)
}
#[test]
fn old_capability_cannot_enqueue_or_claim_success() {
    let mut manager = Manager::default();
    let mut receiver = manager.connect(false);
    assert!(!manager.available());
    assert!(manager.refresh().is_err());
    assert!(receiver.try_recv().is_err());
    assert!(manager.outgoing.is_empty());
}
#[test]
fn same_name_requires_exact_loaded_recipient_and_keeps_text_literal() {
    let (mut manager, mut receiver, one, two) = ready();
    let body = "@./not-a-file !not-a-command\r\n纯文本";
    assert!(
        manager
            .send("same visible name", body, "name-only")
            .is_err()
    );
    assert!(manager.send(&recipient("3").id, body, "stale").is_err());
    manager.send(&two.id, body, "selected").unwrap();
    let sent = receiver.try_recv().unwrap();
    let Command::Send {
        recipient_id,
        text,
        request_id,
    } = sent.command
    else {
        panic!("send")
    };
    assert_eq!(recipient_id, two.id);
    assert_ne!(recipient_id, one.id);
    assert_eq!(text, body);
    assert_eq!(request_id, "selected");
    assert_eq!(manager.outgoing[0].status, "submitting");
    assert!(manager.outgoing[0].message_id.is_none());
    assert!(receiver.try_recv().is_err());
}
#[test]
fn mismatched_receipt_preserves_draft_and_idempotent_retry_cannot_change_target() {
    let (mut manager, mut receiver, one, two) = ready();
    manager
        .send(&one.id, "preserve this text", "original")
        .unwrap();
    let request = receiver.try_recv().unwrap();
    manager.reply(
        request.id,
        Ok(Reply {
            receipts: vec![receipt(&two, "original", Status::Queued)],
            ..Default::default()
        }),
    );
    assert_eq!(manager.outgoing[0].status, "failed");
    assert_eq!(manager.outgoing[0].text, "preserve this text");
    assert!(
        manager
            .send(&two.id, "preserve this text", "original")
            .is_err()
    );
    assert!(manager.send(&one.id, "different", "original").is_err());
    manager
        .send(&one.id, "preserve this text", "original")
        .unwrap();
    let retry = receiver.try_recv().unwrap();
    assert!(retry.id > request.id);
    let Command::Send { request_id, .. } = retry.command else {
        panic!("retry")
    };
    assert_eq!(request_id, "original");
    assert_eq!(manager.outgoing.len(), 1);
}
#[test]
fn asynchronous_receipt_revision_advances_without_new_input() {
    let (mut manager, mut receiver, one, _) = ready();
    manager.send(&one.id, "text", "receipt").unwrap();
    let request = receiver.try_recv().unwrap();
    let before = manager.revision;
    manager.reply(
        request.id,
        Ok(Reply {
            receipts: vec![receipt(&one, "receipt", Status::Queued)],
            ..Default::default()
        }),
    );
    assert!(manager.revision > before);
    assert_eq!(manager.snapshot()["outgoing"][0]["status"], "queued");
    for status in [Status::Included, Status::Acknowledged] {
        manager.refresh().unwrap();
        let list = receiver.try_recv().unwrap();
        let revision = manager.revision;
        manager.reply(
            list.id,
            Ok(Reply {
                recipients: vec![one.clone()],
                receipts: vec![receipt(&one, "receipt", status)],
                ..Default::default()
            }),
        );
        assert!(manager.revision > revision);
    }
    assert_eq!(manager.outgoing[0].status, "acknowledged");
    assert_eq!(manager.outgoing[0].text, "text");
}
#[test]
fn timeout_and_late_reply_do_not_discard_or_silently_resend_text() {
    let (mut manager, mut receiver, one, _) = ready();
    manager.send(&one.id, "retained", "timeout").unwrap();
    let request = receiver.try_recv().unwrap();
    manager.pending.get_mut(&request.id).unwrap().deadline =
        Instant::now() - Duration::from_secs(1);
    manager.next_poll = Some(Instant::now() + POLL);
    let before = manager.revision;
    manager.poll();
    assert!(manager.revision > before);
    assert_eq!(manager.outgoing[0].status, "failed");
    assert_eq!(manager.outgoing[0].text, "retained");
    let revision = manager.revision;
    manager.reply(
        request.id,
        Ok(Reply {
            receipts: vec![receipt(&one, "timeout", Status::Queued)],
            ..Default::default()
        }),
    );
    assert_eq!(manager.revision, revision);
    assert_eq!(manager.outgoing[0].status, "failed");
    assert!(receiver.try_recv().is_err());
}
#[test]
fn reconnect_clears_stale_recipients_without_replaying_unconfirmed_send() {
    let (mut manager, mut receiver, one, _) = ready();
    manager.send(&one.id, "do not replay", "reconnect").unwrap();
    let old = receiver.try_recv().unwrap();
    let mut next = manager.connect(true);
    assert!(manager.recipients.is_empty());
    assert_eq!(manager.outgoing[0].status, "failed");
    assert_eq!(manager.outgoing[0].text, "do not replay");
    let list = next.try_recv().unwrap();
    assert!(list.id > old.id);
    assert!(matches!(list.command, Command::List {}));
    assert!(next.try_recv().is_err());
    assert!(manager.send(&one.id, "do not replay", "reconnect").is_err());
    manager.reply(
        old.id,
        Ok(Reply {
            receipts: vec![receipt(&one, "reconnect", Status::Queued)],
            ..Default::default()
        }),
    );
    assert_eq!(manager.outgoing[0].status, "failed");
}
