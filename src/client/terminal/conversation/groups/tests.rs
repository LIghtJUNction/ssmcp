use super::*;
use crate::client::terminal::{Layout, Ui, preview_state};

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
        original: state.chat_saved_draft(),
        revision: state.conversation.composer.revision(),
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
    assert!(state.conversation.groups.retry.is_some());
    assert!(state.notice.as_deref().unwrap().contains("1/2"));
    state.conversation.groups.pending = state.conversation.groups.retry.take();
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
                .conversation
                .groups
                .selected
                .remove(&recipient(2, false).id);
        } else {
            state.conversation.composer.set_text("newer draft");
        }
        let expected = state.conversation.composer.text().to_owned();
        state.chat_poll_groups(&receipts("queued"));
        assert_eq!(state.conversation.composer.text(), expected);
        assert!(state.notice.as_deref().unwrap().contains("draft retained"));
    }
}
