use super::*;
use crate::client::terminal::{Layout, Ui, preview_state};
use std::time::Instant;

fn native(client: &str, agent: Option<&str>, name: &str) -> Value {
    let id = agent.map_or_else(
        || format!("client:{client}"),
        |agent| format!("agent:{client}:{agent}"),
    );
    json!({"kind":"native","native_kind":if agent.is_some() {"agent"} else {"account_inbox"},
        "id":id,"client_id":client,"agent_id":agent,"name":name,
        "account":{"provider":"github","subject":"42","login":"owner"},
        "online":true,"sendable":true})
}

fn wire(entry: Value) -> Value {
    json!({"kind":entry["native_kind"],"id":entry["id"],"client_id":entry["client_id"],
        "agent_id":entry["agent_id"],"display_name":entry["name"],"account":entry["account"],
        "online":true,"sendable":true})
}

#[test]
fn session_hierarchy_keeps_exact_process_and_thread_ids_without_duplicate_local_rows() {
    let own = "1".repeat(64);
    let remote = "2".repeat(64);
    let first = format!("agent_{}", "a".repeat(32));
    let second = format!("agent_{}", "b".repeat(32));
    let local = vec![
        json!({"agent_id":"next","name":"Next caller"}),
        json!({"agent_id":first,"name":"Builder"}),
        json!({"kind":"observed","key":"observed","name":"Caller","run_id":"observed-run"}),
    ];
    let entries = arrange(
        local,
        vec![
            native(&remote, Some(&second), "Remote review"),
            native(&own, Some(&first), "Builder"),
            native(&remote, None, "Remote process"),
            native(&own, None, "Local process"),
        ],
        &own,
    );
    assert_eq!(entries.len(), 5);
    assert_eq!(entries[0]["session_role"], "process");
    assert_eq!(entries[0]["client_id"], own);
    assert_eq!(entries[1]["session_role"], "thread");
    assert_eq!(entries[1]["agent_id"], first);
    assert_eq!(
        group_id(&entries[1]),
        Some(format!("agent:{own}:{first}").as_str())
    );
    assert!(
        group_id(&entries[2]).is_none(),
        "observed callers are never message recipients"
    );
    assert_eq!(entries[3]["session_role"], "process");
    assert_eq!(entries[3]["client_id"], remote);
    assert_eq!(entries[4]["session_role"], "thread");
    assert_eq!(entries[4]["agent_id"], second);
    assert_eq!(entries[4]["session_depth"], 1);
}

#[test]
fn process_header_without_an_advertised_inbox_is_not_a_wildcard_recipient() {
    let id = "3".repeat(64);
    let thread = format!("agent_{}", "a".repeat(32));
    let entries = arrange(
        vec![json!({"agent_id":"next","name":"Next caller"})],
        vec![native(&id, Some(&thread), "Thread only")],
        "",
    );
    assert_eq!(entries[1]["kind"], "process");
    assert_eq!(entries[1]["targetable"], false);
    assert!(group_id(&entries[1]).is_none());
    assert!(group_id(&entries[2]).is_some());
}

#[test]
fn session_process_thread_group_frames_show_ids_and_explicit_selection() {
    for columns in [120, 48] {
        let own = "1".repeat(64);
        let remote = "2".repeat(64);
        let builder = format!("agent_{}", "a".repeat(32));
        let reviewer = format!("agent_{}", "b".repeat(32));
        let mut state = preview_state(
            28,
            columns,
            &Ui {
                layout: Layout::Chat,
                sidebar: false,
                color: true,
                ..Ui::default()
            },
        );
        state.conversation.process_id = own.clone();
        state.conversation.native_fixture = Some(json!({
            "available":true,"connected":true,"outgoing":[],"groups":[],
            "recipients":[wire(native(&own,None,"ssmcp")),
                wire(native(&own,Some(&builder),"Builder thread")),
                wire(native(&remote,None,"Project Z process")),
                wire(native(&remote,Some(&reviewer),"Review thread"))]
        }));
        state
            .chat_group_command(&format!("add client:{remote} agent:{own}:{builder}"))
            .unwrap();
        state.chat_change_view(View::Agents);
        state.conversation.content_focus = true;
        state.conversation.selected_agent = Some(format!("native:agent:{own}:{builder}"));
        let frame = state.chat_frame(Instant::now());
        let bytes = frame.diff(None);
        let mut parser = vt100::Parser::new(
            std::num::NonZeroU16::new(frame.geometry.rows).unwrap(),
            std::num::NonZeroU16::new(frame.geometry.cols).unwrap(),
            0,
        );
        parser.process(&bytes);
        let contents = parser.screen().contents();
        assert!(contents.contains("Sessions"));
        assert!(contents.contains("process"));
        assert!(contents.contains("thread"));
        assert!(contents.contains("[x]"));
        assert!(contents.contains("#aaaaaaaaaa"));
        assert!(contents.contains("#2222222222"));
        if columns == 120 {
            assert!(
                contents.contains(&own),
                "full process ID is visible in the detail pane"
            );
            assert!(
                contents.contains(&builder),
                "full thread ID is visible in the detail pane"
            );
        }
        if let Some(directory) = std::env::var_os("SSMCP_INTERACTION_PREVIEW_DIR") {
            let directory = std::path::PathBuf::from(directory);
            std::fs::create_dir_all(&directory).unwrap();
            let name = format!("session-groups-{columns}");
            std::fs::write(directory.join(format!("{name}.ansi")), bytes).unwrap();
            std::fs::write(directory.join(format!("{name}.json")),serde_json::to_vec(&json!({
                "rows":frame.geometry.rows,"cols":frame.geometry.cols,
                "source":"production conversation Frame with synthetic authenticated recipient fixtures"
            })).unwrap()).unwrap();
        }
    }
}

#[test]
fn native_picker_can_select_a_local_process_merged_with_the_user_queue_row() {
    let own = "1".repeat(64);
    let mut state = preview_state(
        28,
        120,
        &Ui {
            layout: Layout::Chat,
            sidebar: false,
            ..Ui::default()
        },
    );
    state.conversation.process_id = own.clone();
    state.conversation.native_fixture = Some(json!({
        "available":true,"connected":true,"outgoing":[],
        "recipients":[wire(native(&own,None,"current process"))]
    }));
    state.chat_native_picker("").unwrap();
    assert!(
        state.conversation.native_picker,
        "restoring the column must not replace explicit picker intent"
    );
    assert_eq!(
        state.conversation.selected_agent.as_deref(),
        Some("member:next")
    );
    state.chat_open(false);
    assert_eq!(
        state.conversation.native_recipient,
        Some(format!("client:{own}"))
    );
    assert_eq!(
        state.conversation.native_account,
        Some(("github".into(), "42".into()))
    );
    assert_eq!(state.conversation.view, View::Activity);
}
