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

#[test]
fn idle_picker_refreshes_only_while_visible_and_unchanged_reply_does_not_redraw() {
    let (mut manager, mut receiver, one, two) = ready();
    assert!(manager.outgoing.is_empty());
    manager.watch_recipients = true;
    let before = manager.revision;
    manager.next_poll = Some(Instant::now());
    manager.poll();
    let list = receiver.try_recv().unwrap();
    assert!(matches!(list.command, Command::List {}));
    assert_eq!(manager.snapshot()["pending"], false);
    assert_eq!(manager.revision, before);
    manager.reply(
        list.id,
        Ok(Reply {
            recipients: vec![one.clone(), two.clone()],
            ..Default::default()
        }),
    );
    assert_eq!(manager.revision, before);
    manager.next_poll = Some(Instant::now());
    manager.poll();
    let next = receiver.try_recv().unwrap();
    manager.reply(
        next.id,
        Ok(Reply {
            recipients: vec![one, two, recipient("3")],
            ..Default::default()
        }),
    );
    assert!(manager.revision > before);
    assert_eq!(manager.recipients.len(), 3);
    manager.watch_recipients = false;
    manager.next_poll = Some(Instant::now());
    manager.poll();
    assert!(receiver.try_recv().is_err());
}

#[test]
fn overlapping_poll_and_send_replies_cannot_regress_delivery_progress() {
    for final_status in [Status::Included, Status::Acknowledged, Status::Expired] {
        let (mut manager, mut receiver, one, _) = ready();
        manager.send(&one.id, "deliver once", "overlap").unwrap();
        let send = receiver.try_recv().unwrap();
        manager.refresh().unwrap();
        let poll = receiver.try_recv().unwrap();
        let mut confirmed = receipt(&one, "overlap", final_status);
        confirmed.updated_at_ms = 20;
        manager.reply(
            poll.id,
            Ok(Reply {
                recipients: vec![one.clone()],
                receipts: vec![confirmed],
                ..Default::default()
            }),
        );
        let expected = manager.outgoing[0].status.clone();
        let mut delayed = receipt(&one, "overlap", Status::Queued);
        delayed.updated_at_ms = 10;
        manager.reply(
            send.id,
            Ok(Reply {
                receipts: vec![delayed],
                ..Default::default()
            }),
        );
        assert_eq!(manager.outgoing[0].status, expected);
        assert_eq!(manager.outgoing[0].confirmed_status, Some(final_status));
        assert_eq!(manager.outgoing[0].text, "deliver once");
        assert!(manager.pending.is_empty());
        assert!(
            receiver.try_recv().is_err(),
            "old replies never replay a send"
        );
    }
}

#[test]
fn confirmed_identity_and_terminal_receipts_survive_stale_or_conflicting_updates() {
    let (mut manager, mut receiver, one, _) = ready();
    manager
        .send(&one.id, "stable identity", "identity")
        .unwrap();
    let sent = receiver.try_recv().unwrap();
    let mut confirmed = receipt(&one, "identity", Status::Included);
    confirmed.updated_at_ms = 20;
    manager.reply(
        sent.id,
        Ok(Reply {
            receipts: vec![confirmed.clone()],
            ..Default::default()
        }),
    );
    let mut replacement = confirmed.clone();
    replacement.message_id = format!("native_{}", "b".repeat(32));
    replacement.status = Status::Acknowledged;
    replacement.updated_at_ms = 30;
    assert!(!manager.update_receipt(replacement));
    assert_eq!(
        manager.outgoing[0].message_id.as_deref(),
        Some(confirmed.message_id.as_str())
    );
    assert_eq!(manager.outgoing[0].status, "included");

    // Even a newer timestamp cannot retract a delivered milestone.
    let mut queued = receipt(&one, "identity", Status::Queued);
    queued.updated_at_ms = 30;
    assert!(!manager.update_receipt(queued));
    assert_eq!(manager.outgoing[0].status, "included");

    let mut acknowledged = confirmed.clone();
    acknowledged.status = Status::Acknowledged;
    acknowledged.updated_at_ms = 30;
    assert!(manager.update_receipt(acknowledged));
    let mut expired = confirmed;
    expired.status = Status::Expired;
    expired.updated_at_ms = 40;
    assert!(!manager.update_receipt(expired));
    assert_eq!(manager.outgoing[0].status, "acknowledged");
    assert!(manager.outgoing[0].error.is_none());
}

#[test]
fn late_send_error_does_not_retract_a_confirmed_receipt() {
    for status in [Status::Queued, Status::Included, Status::Acknowledged] {
        let (mut manager, mut receiver, one, _) = ready();
        manager
            .send(&one.id, "already received", "late-error")
            .unwrap();
        let send = receiver.try_recv().unwrap();
        manager.refresh().unwrap();
        let poll = receiver.try_recv().unwrap();
        manager.reply(
            poll.id,
            Ok(Reply {
                recipients: vec![one.clone()],
                receipts: vec![receipt(&one, "late-error", status)],
                ..Default::default()
            }),
        );
        let before = manager.outgoing[0].status.clone();
        manager.reply(send.id, Err("response lost after dispatch".into()));
        assert_eq!(manager.outgoing[0].status, before);
        assert_eq!(manager.outgoing[0].confirmed_status, Some(status));
        assert!(manager.pending.is_empty());
        assert!(receiver.try_recv().is_err());
    }
}

#[test]
fn disconnect_keeps_included_evidence_without_polling_a_closed_connection_forever() {
    let (mut manager, mut receiver, one, _) = ready();
    manager
        .send(&one.id, "included before disconnect", "closed")
        .unwrap();
    let send = receiver.try_recv().unwrap();
    manager.reply(
        send.id,
        Ok(Reply {
            receipts: vec![receipt(&one, "closed", Status::Included)],
            ..Default::default()
        }),
    );
    let mut next = manager.connect(true);
    assert_eq!(manager.outgoing[0].status, "included");
    assert!(manager.outgoing[0].connection_closed);
    assert!(
        manager.outgoing[0]
            .error
            .as_deref()
            .unwrap()
            .contains("已包含")
    );
    assert_eq!(manager.outgoing[0].text, "included before disconnect");
    let refresh = next.try_recv().unwrap();
    assert!(matches!(refresh.command, Command::List {}));
    manager.reply(
        refresh.id,
        Ok(Reply {
            recipients: vec![one.clone()],
            ..Default::default()
        }),
    );
    manager.next_poll = Some(Instant::now());
    manager.poll();
    assert!(
        next.try_recv().is_err(),
        "closed included messages do not poll forever"
    );

    // Retained evidence must not permanently exhaust the bounded outbox.
    let old = manager.outgoing[0].clone();
    manager.outgoing = (0..128)
        .map(|index| Outgoing {
            request_id: format!("closed-{index}"),
            ..old.clone()
        })
        .collect();
    manager.send(&one.id, "new request", "fresh").unwrap();
    assert_eq!(manager.outgoing.len(), 128);
    assert_eq!(manager.outgoing.last().unwrap().request_id, "fresh");
    let fresh = next.try_recv().unwrap();
    assert!(matches!(fresh.command, Command::Send { .. }));
    assert!(
        next.try_recv().is_err(),
        "only the explicitly requested new send is dispatched"
    );
}

#[test]
fn expired_receipts_are_terminal_and_do_not_retry_the_completed_request() {
    let (mut manager, mut receiver, one, _) = ready();
    manager
        .send(&one.id, "expired body", "expired-request")
        .unwrap();
    let send = receiver.try_recv().unwrap();
    manager.reply(
        send.id,
        Ok(Reply {
            receipts: vec![receipt(&one, "expired-request", Status::Expired)],
            ..Default::default()
        }),
    );
    assert_eq!(manager.outgoing[0].status, "expired");
    manager
        .send(&one.id, "expired body", "expired-request")
        .unwrap();
    assert!(matches!(
        receiver.try_recv().unwrap().command,
        Command::List {}
    ));
    assert!(receiver.try_recv().is_err());
    assert!(!manager.update_receipt(receipt(&one, "expired-request", Status::Queued)));
    assert_eq!(manager.outgoing[0].status, "expired");
}

#[test]
fn conflicting_message_identity_ends_an_explicit_retry_without_claiming_delivery() {
    let (mut manager, mut receiver, one, _) = ready();
    manager
        .send(&one.id, "original body", "same-request")
        .unwrap();
    let send = receiver.try_recv().unwrap();
    let original = receipt(&one, "same-request", Status::Queued);
    manager.reply(
        send.id,
        Ok(Reply {
            receipts: vec![original.clone()],
            ..Default::default()
        }),
    );
    let mut next = manager.connect(true);
    let refresh = next.try_recv().unwrap();
    manager.reply(
        refresh.id,
        Ok(Reply {
            recipients: vec![one.clone()],
            ..Default::default()
        }),
    );
    manager
        .send(&one.id, "original body", "same-request")
        .unwrap();
    let retry = next.try_recv().unwrap();
    let mut conflict = original.clone();
    conflict.message_id = format!("native_{}", "b".repeat(32));
    manager.reply(
        retry.id,
        Ok(Reply {
            receipts: vec![conflict],
            ..Default::default()
        }),
    );
    assert!(manager.pending.is_empty());
    assert_eq!(manager.outgoing[0].status, "failed");
    assert_eq!(
        manager.outgoing[0].message_id.as_deref(),
        Some(original.message_id.as_str())
    );
    assert!(
        manager.outgoing[0]
            .error
            .as_deref()
            .unwrap()
            .contains("不同消息")
    );
    assert_eq!(manager.outgoing[0].text, "original body");
    assert!(next.try_recv().is_err());
}
