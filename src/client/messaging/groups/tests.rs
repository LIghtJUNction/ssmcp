use super::*;
use crate::wire::{ControlAccount, messages::RecipientKind};
use std::collections::BTreeSet;

fn recipient(index: u64, thread: bool) -> Recipient {
    let client_id = format!("{index:064x}");
    let agent_id = thread.then(|| format!("agent_{index:032x}"));
    Recipient {
        id: agent_id.as_ref().map_or_else(
            || format!("client:{client_id}"),
            |agent| format!("agent:{client_id}:{agent}"),
        ),
        client_id,
        agent_id,
        display_name: "Same visible name".into(),
        kind: if thread {
            RecipientKind::Agent
        } else {
            RecipientKind::AccountInbox
        },
        account: ControlAccount {
            provider: "github".into(),
            subject: "42".into(),
            login: "alice".into(),
        },
        online: true,
        sendable: true,
    }
}

fn ready(recipients: Vec<Recipient>) -> (Manager, mpsc::Receiver<Request>) {
    let mut manager = Manager::default();
    let mut receiver = manager.connect(true);
    let list = receiver.try_recv().unwrap();
    manager.reply(
        list.id,
        Ok(Reply {
            recipients,
            ..Default::default()
        }),
    );
    (manager, receiver)
}

fn accepted(request: &Request, index: u64) -> Receipt {
    let Command::Send {
        recipient_id,
        request_id,
        ..
    } = &request.command
    else {
        panic!("send")
    };
    Receipt {
        request_id: request_id.clone(),
        recipient_id: recipient_id.clone(),
        message_id: format!("native_{index:032x}"),
        status: Status::Queued,
        created_at_ms: 1,
        updated_at_ms: 2,
        included_at_ms: None,
        acknowledged_at_ms: None,
        duplicate: false,
    }
}

#[test]
fn arbitrary_process_and_thread_group_binds_the_complete_selection_and_text() {
    let recipients = vec![recipient(1, false), recipient(1, true), recipient(2, false)];
    let (mut manager, mut receiver) = ready(recipients.clone());
    let body = "  @literal $literal\r\n正文  ";
    manager
        .group_send(&recipients, body, "mixed-group")
        .unwrap();
    let mut sent = BTreeSet::new();
    for _ in 0..3 {
        let request = receiver.try_recv().unwrap();
        let Command::Send {
            recipient_id, text, ..
        } = request.command
        else {
            panic!("send")
        };
        sent.insert(recipient_id);
        assert_eq!(text, body);
    }
    assert_eq!(sent, recipients.iter().map(|r| r.id.clone()).collect());
    let mut reordered = recipients.clone();
    reordered.reverse();
    manager.group_send(&reordered, body, "mixed-group").unwrap();
    assert!(receiver.try_recv().is_err());
    assert!(
        manager
            .group_send(&recipients[..2], body, "mixed-group")
            .is_err()
    );
    assert!(
        manager
            .group_send(&recipients, "changed", "mixed-group")
            .is_err()
    );
    let mut another_account = recipients;
    another_account[0].account.subject = "different".into();
    assert!(
        manager
            .group_send(&another_account, body, "mixed-group")
            .is_err()
    );
    assert_eq!(manager.group_snapshots().len(), 1);
}

#[test]
fn group_drains_all_32_targets_through_the_bounded_transport_without_dropping_them() {
    let recipients: Vec<_> = (1..=32).map(|index| recipient(index, false)).collect();
    let (mut manager, mut receiver) = ready(recipients.clone());
    manager
        .group_send(&recipients, "all selected", "all-32")
        .unwrap();
    assert_eq!(manager.pending.len(), 16);
    assert_eq!(
        manager.group_snapshots()[0]["targets"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|target| target["status"] == "waiting")
            .count(),
        16
    );
    let mut requests = BTreeSet::new();
    for index in 1..=32 {
        let request = receiver.try_recv().unwrap();
        let receipt = accepted(&request, index);
        assert!(requests.insert(receipt.request_id.clone()));
        manager.reply(
            request.id,
            Ok(Reply {
                receipts: vec![receipt],
                ..Default::default()
            }),
        );
        assert!(manager.pending.len() <= 16);
    }
    assert_eq!(requests.len(), 32);
    assert!(receiver.try_recv().is_err());
    assert!(manager.pending.is_empty());
    let groups = manager.group_snapshots();
    assert!(
        groups[0]["targets"]
            .as_array()
            .unwrap()
            .iter()
            .all(|target| target["status"] == "queued")
    );
}

#[test]
fn partial_group_retry_keeps_child_ids_and_never_resends_an_accepted_target() {
    let recipients = vec![recipient(1, false), recipient(2, true)];
    let (mut manager, mut receiver) = ready(recipients.clone());
    manager
        .group_send(&recipients, "once per target", "partial")
        .unwrap();
    let one = receiver.try_recv().unwrap();
    let two = receiver.try_recv().unwrap();
    let confirmed = accepted(&one, 1);
    manager.reply(
        one.id,
        Ok(Reply {
            receipts: vec![confirmed.clone()],
            ..Default::default()
        }),
    );
    manager.reply(two.id, Err("transport response unknown".into()));
    assert!(receiver.try_recv().is_err(), "failure does not auto-retry");
    manager.group_retry("partial").unwrap();
    let retry = receiver.try_recv().unwrap();
    assert_eq!(retry.command, two.command);
    assert_ne!(retry.id, two.id);
    assert!(
        receiver.try_recv().is_err(),
        "accepted target is not resent"
    );
    let second = accepted(&retry, 2);
    manager.reply(
        retry.id,
        Ok(Reply {
            receipts: vec![second],
            ..Default::default()
        }),
    );
    assert!(manager.group_retry("partial").is_err());
    let groups = manager.group_snapshots();
    assert!(
        groups[0]["targets"]
            .as_array()
            .unwrap()
            .iter()
            .any(|target| target["message_id"] == confirmed.message_id)
    );
}

#[test]
fn failed_status_query_cannot_rearm_a_service_accepted_queued_child() {
    let recipients = vec![recipient(1, false)];
    let (mut manager, mut receiver) = ready(recipients.clone());
    manager
        .group_send(&recipients, "one dispatch", "queued-child")
        .unwrap();
    let sent = receiver.try_recv().unwrap();
    let confirmed = accepted(&sent, 1);
    manager.reply(sent.id, Err("send response lost".into()));

    // A retry encounters a local error before the delayed service receipt is
    // observed. This leaves the child marked failed even after confirmation.
    manager.recipients[0].online = false;
    manager.group_retry("queued-child").unwrap();
    assert!(receiver.try_recv().is_err());
    manager.recipients[0].online = true;

    let status = Command::Status {
        message_id: confirmed.message_id.clone(),
    };
    manager.request(status.clone()).unwrap();
    let query = receiver.try_recv().unwrap();
    assert_eq!(query.command, status);
    manager.reply(
        query.id,
        Ok(Reply {
            receipts: vec![confirmed.clone()],
            ..Default::default()
        }),
    );
    manager.request(status.clone()).unwrap();
    let query = receiver.try_recv().unwrap();
    assert_eq!(query.command, status);
    manager.reply(query.id, Err("status response lost".into()));
    assert_eq!(manager.error.as_deref(), Some("status response lost"));

    let groups = manager.group_snapshots();
    let child = &groups[0]["targets"][0];
    assert_eq!(child["status"], "failed");
    assert_eq!(child["confirmed_status"], "queued");
    assert_eq!(child["message_id"], confirmed.message_id);
    assert_eq!(child["connection_closed"], false);
    let error = manager.group_retry("queued-child").unwrap_err();
    assert!(error.contains("No unconfirmed failed targets"));
    assert!(
        receiver.try_recv().is_err(),
        "the accepted child was not rearmed"
    );
}

#[test]
fn a_disconnected_group_does_not_resume_or_rebind_after_reconnect() {
    let recipients: Vec<_> = (1..=32).map(|index| recipient(index, false)).collect();
    let (mut manager, _receiver) = ready(recipients.clone());
    manager
        .group_send(&recipients, "frozen selection", "disconnected")
        .unwrap();
    manager.disconnect();
    assert!(
        manager.group_snapshots()[0]["targets"]
            .as_array()
            .unwrap()
            .iter()
            .all(|target| target["status"] == "failed")
    );
    let mut next = manager.connect(true);
    let list = next.try_recv().unwrap();
    manager.reply(
        list.id,
        Ok(Reply {
            recipients: recipients.clone(),
            ..Default::default()
        }),
    );
    manager.poll();
    assert!(next.try_recv().is_err());
    assert!(manager.group_retry("disconnected").is_err());
    assert!(
        manager
            .group_send(&recipients, "frozen selection", "disconnected")
            .is_err()
    );
    assert!(next.try_recv().is_err());
}

#[test]
fn queued_group_targets_recheck_account_before_each_dispatch() {
    let recipients: Vec<_> = (1..=32).map(|index| recipient(index, false)).collect();
    let (mut manager, mut receiver) = ready(recipients.clone());
    manager
        .group_send(&recipients, "same account only", "identity-change")
        .unwrap();
    let changed = recipients[16].id.clone();
    manager
        .recipients
        .iter_mut()
        .find(|r| r.id == changed)
        .unwrap()
        .account
        .subject = "new-owner".into();
    let mut delivered = BTreeSet::new();
    for index in 1..=31 {
        let request = receiver.try_recv().unwrap();
        let receipt = accepted(&request, index);
        delivered.insert(receipt.recipient_id.clone());
        manager.reply(
            request.id,
            Ok(Reply {
                receipts: vec![receipt],
                ..Default::default()
            }),
        );
    }
    assert!(!delivered.contains(&changed));
    assert!(receiver.try_recv().is_err());
    let groups = manager.group_snapshots();
    let failed = groups[0]["targets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|target| target["recipient_id"] == changed)
        .unwrap();
    assert_eq!(failed["status"], "failed");
    assert!(
        failed["error"]
            .as_str()
            .unwrap()
            .contains("account changed")
    );
}
