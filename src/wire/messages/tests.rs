use super::*;
use serde_json::json;
#[test]
fn native_command_rejects_labels_forged_origin_and_noncanonical_shapes() {
    let client = "a".repeat(64);
    let valid = format!("client:{client}");
    assert!(parse_recipient(&valid).is_ok());
    assert!(parse_recipient(&format!("agent:{client}:agent_{}", "b".repeat(32))).is_ok());
    for id in [
        "@oh-my-ztty-1000-0.2.1",
        "same name",
        "next-caller",
        "client:",
        "agent:bad:agent_123",
    ] {
        assert!(parse_recipient(id).is_err());
    }
    assert!(
        serde_json::from_value::<Command>(json!({"action":"send","recipient_id":valid,
        "request_id":"literal","text":"hello","sender_kind":"local_user"}))
        .is_err()
    );
    assert!(
        serde_json::from_value::<Command>(json!({"action":"list","controller":{"name":"human"}}))
            .is_err()
    );
}
#[test]
fn native_text_validation_counts_utf8_and_preserves_literal_content() {
    let literal = "@./path !echo unchanged\r\n你好";
    assert!(validate_text(literal).is_ok());
    for text in ["", "  \n", "before\0after"] {
        assert!(validate_text(text).is_err());
    }
    assert!(validate_text(&"好".repeat(MAX_USER_MESSAGE_BYTES / 3 + 1)).is_err());
    for request in ["", "request with space", "../escape/", "é"] {
        assert!(validate_request_id(request).is_err());
    }
}
#[test]
fn native_receipts_require_valid_ids_and_message_recipient_consistency() {
    let recipient = Recipient {
        id: format!("client:{}", "a".repeat(64)),
        client_id: "a".repeat(64),
        agent_id: None,
        display_name: "target".into(),
        kind: RecipientKind::AccountInbox,
        account: ControlAccount {
            provider: "github".into(),
            subject: "42".into(),
            login: "alice".into(),
        },
        online: true,
        sendable: true,
    };
    let receipt = Receipt {
        request_id: "request".into(),
        message_id: format!("native_{}", "b".repeat(32)),
        recipient_id: recipient.id.clone(),
        status: Status::Included,
        created_at_ms: 1,
        updated_at_ms: 2,
        included_at_ms: Some(2),
        acknowledged_at_ms: None,
        duplicate: false,
    };
    let mut reply = Reply {
        receipts: vec![receipt.clone()],
        ..Default::default()
    };
    assert!(reply.validate().is_ok());
    reply.receipts[0].message_id = "display-name".into();
    assert!(reply.validate().is_err());
    reply.receipts.clear();
    reply.messages.push(Message {
        receipt,
        sender: Sender {
            kind: Origin::Agent,
            client_id: "c".repeat(64),
            account: recipient.account.clone(),
            agent: None,
        },
        recipient,
        text: "literal".into(),
    });
    assert!(reply.validate().is_ok());
    reply.messages[0].receipt.recipient_id = format!("client:{}", "d".repeat(64));
    assert!(reply.validate().is_err());
}
