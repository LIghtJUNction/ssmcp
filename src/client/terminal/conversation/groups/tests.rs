use super::*;
use crate::client::messaging::test_connection::Connection;
use crate::client::terminal::{Layout, Ui, preview_state};
use crate::wire::messages::{Command, Receipt, Reply, Status};

pub(super) fn recipient(client: u64, thread: bool) -> Recipient {
    let client_id = format!("{client:064x}");
    let agent_id = thread.then(|| format!("agent_{client:032x}"));
    Recipient {
        id: agent_id.as_ref().map_or_else(
            || format!("client:{client_id}"),
            |agent| format!("agent:{client_id}:{agent}"),
        ),
        client_id,
        agent_id,
        display_name: "Repeated name".into(),
        kind: if thread {
            crate::wire::messages::RecipientKind::Agent
        } else {
            crate::wire::messages::RecipientKind::AccountInbox
        },
        account: crate::wire::ControlAccount {
            provider: "github".into(),
            subject: "42".into(),
            login: "owner".into(),
        },
        online: true,
        sendable: true,
    }
}

pub(super) fn state() -> State {
    let mut state = preview_state(
        28,
        120,
        &Ui {
            layout: Layout::Chat,
            sidebar: false,
            ..Ui::default()
        },
    );
    state.conversation.process_id = format!("{:064x}", 1);
    state.conversation.native_fixture = Some(json!({
        "available":true,"connected":true,"outgoing":[],"groups":[],
        "recipients":[recipient(1,false),recipient(1,true),recipient(2,false)]
    }));
    state
}

#[test]
fn group_command_parsing_preserves_literal_body_and_requires_exact_command_boundary() {
    let body = "  @new-file $skill\r\n正文  ";
    let text = format!("/group send {body}");
    let arguments = arguments(&text).unwrap();
    let (action, rest) = arguments.split_once(char::is_whitespace).unwrap();
    assert_eq!(action, "send");
    assert_eq!(rest, body);
    assert!(super::arguments("/grouped send text").is_none());
    assert!(retains_draft(&text));
    assert!(retains_draft("/group retry"));
    assert!(!retains_draft("/group clear"));
}

#[test]
fn arbitrary_groups_mix_processes_and_threads_and_named_groups_keep_exact_ids() {
    let mut state = state();
    let one = recipient(1, false);
    let thread = recipient(1, true);
    let two = recipient(2, false);
    state
        .chat_group_command(&format!("add {} {} {}", one.id, thread.id, two.id))
        .unwrap();
    assert_eq!(state.conversation.groups.selected.len(), 3);
    state.chat_group_command("save review team").unwrap();
    state.chat_group_command("clear").unwrap();
    assert!(state.conversation.groups.selected.is_empty());
    state.chat_group_command("use review team").unwrap();
    assert_eq!(state.conversation.groups.selected.len(), 3);
    let before: Vec<_> = state.conversation.groups.selected.keys().cloned().collect();
    assert!(state.chat_group_command("add Repeated name").is_err());
    assert!(state.chat_group_command("add *").is_err());
    assert_eq!(
        state
            .conversation
            .groups
            .selected
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        before
    );
    state
        .chat_group_command(&format!("remove {}", thread.id))
        .unwrap();
    assert_eq!(state.conversation.groups.selected.len(), 2);
    assert!(state.conversation.groups.selected.contains_key(&one.id));
    assert!(state.conversation.groups.selected.contains_key(&two.id));
}

#[test]
fn group_selection_changes_are_atomic_and_account_changes_require_reselection() {
    let mut state = state();
    let one = recipient(1, false);
    state
        .chat_group_command(&format!("add {}", one.id))
        .unwrap();
    state.chat_group_command("save pinned").unwrap();
    assert!(
        state
            .chat_group_command(&format!("add {} unknown-target", recipient(2, false).id))
            .is_err()
    );
    assert_eq!(state.conversation.groups.selected.len(), 1);
    state.conversation.native_fixture.as_mut().unwrap()["recipients"][0]["account"]["subject"] =
        json!("new-account");
    assert!(state.group_current_selection().is_err());
    assert!(state.chat_group_command("use pinned").is_err());
    assert_eq!(
        state.conversation.groups.selected[&one.id].account.subject,
        "42"
    );
    state.chat_group_command("clear").unwrap();
    assert!(state.chat_group_command("send retained text").is_err());
}

#[test]
fn space_selects_only_one_exact_process_and_g_preserves_the_message_draft() {
    let mut state = state();
    state.conversation.composer.set_text("existing draft");
    state.chat_change_view(View::Agents);
    state.conversation.content_focus = true;
    state.conversation.selected_agent = Some("member:next".into());
    state.chat_character(' ');
    assert_eq!(state.conversation.groups.selected.len(), 1);
    assert!(
        state
            .conversation
            .groups
            .selected
            .contains_key(&recipient(1, false).id)
    );
    assert!(
        !state
            .conversation
            .groups
            .selected
            .contains_key(&recipient(1, true).id)
    );
    assert_eq!(state.conversation.composer.text(), "existing draft");
    state.chat_character('g');
    assert_eq!(state.conversation.view, View::Activity);
    assert_eq!(
        state.conversation.composer.text(),
        "/group send existing draft"
    );
    assert!(!state.conversation.content_focus);
    assert!(
        state.chat_group_command("send existing draft").is_err(),
        "preview cannot send"
    );
    assert_eq!(
        state.conversation.composer.text(),
        "/group send existing draft"
    );
}

fn pending(state: &State) -> Pending {
    Pending {
        request_id: "ui-group-fixture".into(),
        recipients: state
            .conversation
            .groups
            .selected
            .values()
            .cloned()
            .collect(),
        body: "message body".into(),
        stamp: state.chat_composer_stamp(),
    }
}

fn receipts(second: &str) -> Value {
    json!({"groups":[{
        "request_id":"ui-group-fixture","text":"message body",
        "targets":[{"recipient_id":recipient(1,false).id,"status":"queued"},
            {"recipient_id":recipient(2,false).id,"status":second}]
    }]})
}

#[test]
fn partial_group_receipts_keep_the_draft_and_only_full_confirmation_clears_it() {
    let mut state = state();
    state
        .chat_group_command(&format!(
            "add {} {}",
            recipient(1, false).id,
            recipient(2, false).id
        ))
        .unwrap();
    state
        .conversation
        .composer
        .set_text("/group send message body");
    state.conversation.groups.pending = Some(pending(&state));
    state.chat_poll_groups(&receipts("failed"));
    assert_eq!(
        state.conversation.composer.text(),
        "/group send message body"
    );
    assert!(state.conversation.groups.pending.is_none());
    assert!(
        state
            .conversation
            .groups
            .retry
            .contains_key("ui-group-fixture")
    );
    assert!(state.notice.as_deref().unwrap().contains("1/2"));
    state.conversation.groups.pending = state.conversation.groups.retry.remove("ui-group-fixture");
    state.chat_poll_groups(&receipts("queued"));
    assert!(state.conversation.composer.is_empty());
    assert!(state.notice.as_deref().unwrap().contains("2/2"));
}

#[test]
fn group_confirmation_never_clears_a_newer_draft_or_a_changed_selection() {
    for change_selection in [false, true] {
        let mut state = state();
        state
            .chat_group_command(&format!(
                "add {} {}",
                recipient(1, false).id,
                recipient(2, false).id
            ))
            .unwrap();
        state
            .conversation
            .composer
            .set_text("/group send message body");
        state.conversation.groups.pending = Some(pending(&state));
        if change_selection {
            state
                .chat_group_command(&format!("remove {}", recipient(2, false).id))
                .unwrap();
        } else {
            state.conversation.composer.set_text("newer draft");
        }
        let expected = state.conversation.composer.text().to_owned();
        state.chat_poll_groups(&receipts("queued"));
        assert_eq!(state.conversation.composer.text(), expected);
        assert!(state.notice.as_deref().unwrap().contains("draft retained"));
    }
}

fn connected() -> State {
    let mut state = state();
    state.conversation.native_fixture = None;
    state.conversation.native_connection = Some(Connection::new(vec![
        recipient(1, false),
        recipient(1, true),
        recipient(2, false),
    ]));
    state
}

fn connection(state: &mut State) -> &mut Connection {
    state.conversation.native_connection.as_mut().unwrap()
}

fn choose_native(state: &mut State, recipient: &Recipient) {
    state.chat_native_picker(&recipient.id).unwrap();
    state.chat_open(true);
    assert_eq!(
        state.conversation.native_recipient.as_deref(),
        Some(recipient.id.as_str())
    );
}

fn choose_local(state: &mut State) {
    state.chat_command("/sessions").unwrap();
    state.conversation.selected_agent = Some("member:next".into());
    state.chat_open(true);
    assert_eq!(state.conversation.view, View::Activity);
    assert!(state.conversation.native_recipient.is_none());
    assert!(!state.conversation.send_to_group);
}

fn accept(state: &mut State, id: u64, command: &Command) {
    let Command::Send {
        recipient_id,
        request_id,
        ..
    } = command
    else {
        panic!("expected a native Send command");
    };
    connection(state).reply(
        id,
        Ok(Reply {
            receipts: vec![Receipt {
                recipient_id: recipient_id.clone(),
                request_id: request_id.clone(),
                message_id: format!("native_{id:032x}"),
                status: Status::Queued,
                created_at_ms: 1,
                updated_at_ms: 2,
                included_at_ms: None,
                acknowledged_at_ms: None,
                duplicate: false,
            }],
            ..Reply::default()
        }),
    );
    state.chat_poll_native();
}

fn assert_only_native(state: &State) {
    assert!(state.conversation.force_task.is_none());
    assert!(state.conversation.submitting.is_none());
    assert!(state.actions.is_empty());
}

#[test]
fn send_dispatches_literal_body_to_the_exact_native_process_or_thread() {
    for selected in [recipient(2, false), recipient(1, true)] {
        let mut state = connected();
        // The body starts with this recipient's exact address. Ordinary native
        // submission would strip the mention. /send must keep it literally.
        let body = format!("@{}  中文\n    @missing.txt $missing-skill\n", selected.id);
        let draft = format!("  /send\t{body}");
        state.conversation.composer.set_text(&draft);
        state
            .chat_group_command(&format!("add {}", recipient(1, false).id))
            .unwrap();
        choose_native(&mut state, &selected);
        assert_eq!(state.conversation.groups.selected.len(), 1);

        state.chat_submit();
        assert_eq!(state.conversation.composer.text(), draft);
        assert_only_native(&state);
        let sent = connection(&mut state).next_request().unwrap();
        let Command::Send {
            recipient_id, text, ..
        } = &sent.command
        else {
            panic!("expected a native Send command");
        };
        assert_eq!(recipient_id, &selected.id);
        assert_eq!(text, &body);
        assert!(connection(&mut state).next_request().is_none());
        accept(&mut state, sent.id, &sent.command);
        assert!(state.conversation.composer.is_empty());
    }
}

#[test]
fn send_to_stale_unavailable_unbound_or_wrong_account_native_target_keeps_its_draft() {
    for (condition, notice) in [
        ("missing", "not currently verified"),
        ("offline", "unavailable"),
        ("wrong-account", "account changed"),
        ("changed-after-snapshot", "account changed"),
        ("unbound", "verify its account"),
        ("unsupported", "not connected"),
    ] {
        let mut state = connected();
        let selected = recipient(2, false);
        choose_native(&mut state, &selected);
        let mut snapshot = state.native_snapshot();
        let entries = snapshot["recipients"].as_array_mut().unwrap();
        let index = entries
            .iter()
            .position(|entry| entry["id"] == selected.id)
            .unwrap();
        match condition {
            "missing" => {
                entries.remove(index);
            }
            "offline" => entries[index]["online"] = json!(false),
            "wrong-account" => entries[index]["account"]["subject"] = json!("99"),
            "changed-after-snapshot" => {
                let mut current = selected;
                current.account.subject = "99".into();
                state.conversation.native_connection = Some(Connection::new(vec![current]));
            }
            "unbound" => state.conversation.native_account = None,
            "unsupported" => snapshot["available"] = json!(false),
            _ => unreachable!(),
        }
        state.conversation.native_fixture = Some(snapshot);
        state
            .conversation
            .composer
            .set_text("/send\t中文\n  @missing.txt");
        let before = state.chat_saved_draft();

        state.chat_submit();
        assert!(
            state.notice.as_deref().unwrap().contains(notice),
            "{condition}"
        );
        assert_eq!(state.chat_saved_draft(), before);
        assert!(state.conversation.native_pending.is_none());
        assert!(connection(&mut state).next_request().is_none());
        assert_only_native(&state);
    }
}

#[test]
fn send_to_selected_group_keeps_its_exact_mixed_targets_and_retries_only_the_failed_child() {
    let mut state = connected();
    let thread = recipient(1, true);
    let inbox = recipient(2, false);
    state
        .chat_group_command(&format!("add {} {}", inbox.id, thread.id))
        .unwrap();
    let body = "  @recipient $literal\n    正文  ";
    let draft = format!("/send\n{body}");
    state.conversation.composer.set_text(&draft);

    state.chat_submit();
    let request_id = state
        .conversation
        .groups
        .pending
        .as_ref()
        .unwrap()
        .request_id
        .clone();
    let accepted = connection(&mut state).next_request().unwrap();
    let failed = connection(&mut state).next_request().unwrap();
    for (command, expected) in [(&accepted.command, &thread), (&failed.command, &inbox)] {
        let Command::Send {
            recipient_id, text, ..
        } = command
        else {
            panic!("expected a native Send command");
        };
        assert_eq!(recipient_id, &expected.id);
        assert_eq!(text, body);
    }
    assert!(connection(&mut state).next_request().is_none());
    assert_only_native(&state);
    accept(&mut state, accepted.id, &accepted.command);
    assert_eq!(state.conversation.composer.text(), draft);
    connection(&mut state).reply(failed.id, Err("No service receipt".into()));
    state.chat_poll_native();
    assert_eq!(state.conversation.composer.text(), draft);
    assert!(!state.chat_group_pending());

    // An unrelated send must leave the first partial request available for
    // retry when the original selection and body are used again.
    state.chat_group_command("clear").unwrap();
    state
        .chat_group_command(&format!("add {}", recipient(1, false).id))
        .unwrap();
    state.conversation.composer.set_text("/send unrelated");
    state.chat_submit();
    let unrelated = connection(&mut state).next_request().unwrap();
    connection(&mut state).reply(unrelated.id, Err("No service receipt".into()));
    state.chat_poll_native();
    state.chat_group_command("clear").unwrap();
    state
        .chat_group_command(&format!("add {} {}", inbox.id, thread.id))
        .unwrap();
    state.conversation.composer.set_text(&draft);

    state.chat_submit();
    assert_eq!(
        state
            .conversation
            .groups
            .pending
            .as_ref()
            .unwrap()
            .request_id,
        request_id
    );
    let retry = connection(&mut state).next_request().unwrap();
    assert_eq!(
        retry.command, failed.command,
        "same child ID, recipient and body"
    );
    assert_ne!(
        retry.id, failed.id,
        "a new attempt at the same child request"
    );
    assert!(
        connection(&mut state).next_request().is_none(),
        "no accepted child was resent"
    );
    assert_eq!(state.conversation.composer.text(), draft);
    accept(&mut state, retry.id, &retry.command);
    assert!(state.conversation.composer.is_empty());
    assert_eq!(
        state.native_snapshot()["groups"].as_array().unwrap().len(),
        2
    );
}

#[test]
fn send_uses_an_explicit_local_selection_even_when_a_group_is_available() {
    let mut state = connected();
    state
        .chat_group_command(&format!("add {}", recipient(1, true).id))
        .unwrap();
    choose_local(&mut state);
    let draft = "/send send to the local listener";
    state.conversation.composer.set_text(draft);
    state.chat_submit();
    // The listener branch reaches its preview guard. The isolated native
    // transport would have accepted a group request if selected.
    assert_eq!(state.notice.as_deref(), Some("预览不会发送消息"));
    assert_eq!(state.conversation.composer.text(), draft);
    assert_eq!(state.conversation.groups.selected.len(), 1);
    assert!(connection(&mut state).next_request().is_none());
    assert_only_native(&state);

    let saved_group = state.conversation.groups.reload_snapshot();
    let saved_draft = state.chat_saved_draft();
    state.conversation.groups.restore(saved_group).unwrap();
    state.chat_restore_draft(saved_draft);
    state.chat_poll_native();
    assert_eq!(state.conversation.groups.selected.len(), 1);
    state.chat_submit();
    assert_eq!(state.notice.as_deref(), Some("预览不会发送消息"));
    assert_eq!(state.conversation.composer.text(), draft);
    assert!(connection(&mut state).next_request().is_none());
}

#[test]
fn send_in_empty_or_stale_group_context_never_uses_selected_single_recipient() {
    for clear in [true, false] {
        let mut state = connected();
        choose_native(&mut state, &recipient(1, false));
        let thread = recipient(1, true);
        state
            .chat_group_command(&format!("add {}", thread.id))
            .unwrap();
        if clear {
            state.chat_group_command("clear").unwrap();
        } else {
            let mut snapshot = state.native_snapshot();
            snapshot["recipients"]
                .as_array_mut()
                .unwrap()
                .retain(|entry| entry["id"] != thread.id);
            state.conversation.native_fixture = Some(snapshot);
        }
        state
            .conversation
            .composer
            .set_text("/send must stay in this group");
        let before = state.chat_saved_draft();
        state.chat_submit();
        assert_eq!(state.chat_saved_draft(), before);
        assert!(state.notice.as_deref().unwrap().contains(if clear {
            "group is empty"
        } else {
            "exact recipient"
        }));
        assert!(connection(&mut state).next_request().is_none());
        assert_only_native(&state);
    }
}

#[test]
fn late_send_receipts_preserve_the_draft_after_text_cursor_or_destination_changes() {
    for change in [
        "native",
        "group",
        "membership",
        "local",
        "text",
        "cursor",
        "revision",
    ] {
        let mut state = connected();
        let inbox = recipient(2, false);
        let thread = recipient(1, true);
        choose_native(&mut state, &inbox);
        let add_thread = format!("add {}", thread.id);
        let use_group = matches!(change, "membership" | "local");
        if use_group {
            state.chat_group_command(&add_thread).unwrap();
        }
        let draft = "/send keep the new intention 中文";
        state.conversation.composer.set_text(draft);
        state.chat_submit();
        let sent = connection(&mut state).next_request().unwrap();

        match change {
            "native" => choose_native(&mut state, &thread),
            "group" => state.chat_group_command(&add_thread).unwrap(),
            "membership" => {
                // Returning to the identical membership is still a newer
                // intention, even with unchanged text and cursor.
                state
                    .chat_group_command(&format!("remove {}", thread.id))
                    .unwrap();
                state.chat_group_command(&add_thread).unwrap();
            }
            "local" => choose_local(&mut state),
            "text" => assert!(state.conversation.composer.set_text("new draft")),
            "cursor" => assert!(state.conversation.composer.left()),
            "revision" => assert!(state.conversation.composer.set_text(draft)),
            _ => unreachable!(),
        }
        let newer = state.chat_composer_stamp();
        accept(&mut state, sent.id, &sent.command);
        assert_eq!(state.chat_composer_stamp(), newer, "{change}");
    }
}

#[test]
fn send_waits_for_pending_receipts_and_respects_queue_edits_and_shutdown() {
    let mut state = connected();
    choose_native(&mut state, &recipient(2, false));
    let draft = "/send keep the input";
    state.conversation.composer.set_text(draft);
    state.conversation.editing = Some("queued-message".into());
    state.chat_submit();
    assert!(state.notice.as_deref().unwrap().contains("队列消息"));
    assert!(connection(&mut state).next_request().is_none());
    state.conversation.editing = None;
    state.conversation.exit_requested = true;
    state.chat_submit();
    assert!(state.notice.as_deref().unwrap().contains("关闭"));
    assert!(connection(&mut state).next_request().is_none());
    state.conversation.exit_requested = false;

    state.chat_submit();
    let sent = connection(&mut state).next_request().unwrap();
    state
        .chat_group_command(&format!("add {}", recipient(1, true).id))
        .unwrap();
    state.chat_submit();
    assert!(state.notice.as_deref().unwrap().contains("回执"));
    assert!(connection(&mut state).next_request().is_none());
    assert_eq!(state.conversation.composer.text(), draft);
    accept(&mut state, sent.id, &sent.command);
    assert_eq!(state.conversation.composer.text(), draft);
}
