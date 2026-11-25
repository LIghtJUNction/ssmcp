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

fn screen_contents(frame: &Frame) -> String {
    let mut parser = vt100::Parser::new(
        nonzero(frame.geometry.rows),
        nonzero(frame.geometry.cols),
        0,
    );
    parser.process(&frame.diff(None));
    parser.screen().contents()
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
fn recipient_list_tracks_member_lifecycle_across_updates() {
    for width in [48, 120] {
        let mut state = preview_state(
            28,
            width,
            &Ui {
                layout: Layout::Chat,
                sidebar: false,
                color: false,
                ..Ui::default()
            },
        );
        state.chat_change_view(View::Agents);
        state.conversation.selected_agent = Some("member:worker".into());
        for (status, label) in [
            ("idle", "Idle"),
            ("dreaming", "Dreaming"),
            ("working", "Working"),
        ] {
            let data = json!({"agents":[{"name":"Compiler","agent_id":"worker","status":status}],"queue":[]});
            let rows = state.chat_session_rows(&data, 24).join("\n");
            assert!(rows.contains(&format!("{label} · Compiler")), "{rows}");
            if status != "dreaming" {
                assert!(!rows.contains("Dreaming"), "{rows}");
            }
            if width == 120 {
                assert!(rows.contains(&format!("Status · {label}")), "{rows}");
            }
        }
    }
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
        let contents = screen_contents(&frame);
        assert!(contents.contains("Recipients"));
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
            std::fs::write(directory.join(format!("{name}.ansi")), frame.diff(None)).unwrap();
            std::fs::write(directory.join(format!("{name}.json")),serde_json::to_vec(&json!({
                "rows":frame.geometry.rows,"cols":frame.geometry.cols,
                "source":"production conversation Frame with synthetic authenticated recipient fixtures"
            })).unwrap()).unwrap();
        }
        state.chat_character('i');
        let contents = screen_contents(&state.chat_frame(Instant::now()));
        let joined: String = contents.split_whitespace().collect();
        assert!(joined.contains(&format!("ClientID:{own}")), "{contents}");
        assert!(
            joined.contains(&format!("ThreadID:{builder}")),
            "{contents}"
        );
        assert!(
            joined.contains(&format!("Exactrecipient:agent:{own}:{builder}")),
            "{contents}"
        );
    }
}

#[test]
fn sessions_render_the_wire_role_as_a_summary_in_wide_and_narrow_frames() {
    for columns in [48, 120] {
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
        state.conversation.process_id = "a".repeat(64);
        state.chat_change_view(View::Agents);
        state.conversation.content_focus = true;
        state.conversation.selected_agent = Some("member:example-builder".into());
        let rows = screen_contents(&state.chat_frame(Instant::now()));
        assert!(rows.contains("implementation"), "{rows}");
        state.chat_character('i');
        let rows = screen_contents(&state.chat_frame(Instant::now()));
        assert!(rows.contains("Thread ID: example-builder"), "{rows}");
        assert!(rows.contains("Summary: implementation"), "{rows}");

        let data = json!({"agents":[],"queue":[]});
        let role = format!("**literal** {}\u{001b}[2J\u{202e}", "界".repeat(31));
        let entry = json!({"session_role":"thread","agent_id":"example-builder","name":"Builder",
            "client_id":"a".repeat(64),"role":role});
        let details = state.session_details(&entry, &data);
        let summary_start = details
            .iter()
            .position(|(line, _)| line == "Summary")
            .unwrap();
        let summary: String = details[summary_start + 1..]
            .iter()
            .take_while(|(line, _)| !line.is_empty())
            .map(|(line, _)| line.as_str())
            .collect();
        assert_eq!(summary, panel::safe_text(&role));
        assert!(summary.contains("**literal**"));
        assert!(!summary.contains('\u{001b}'));
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

#[test]
fn history_key_keeps_same_name_threads_and_the_composition_separate() {
    let own = "7".repeat(64);
    let first = format!("agent_{}", "c".repeat(32));
    let second = format!("agent_{}", "d".repeat(32));
    let mut state = preview_state(28, 120, &Ui::default());
    state.conversation.process_id = own.clone();
    state.conversation.native_fixture = Some(json!({
        "available":true,"connected":true,"outgoing":[],"groups":[],
        "recipients":[wire(native(&own, Some(&first), "Same name")),
            wire(native(&own, Some(&second), "Same name"))]
    }));
    state
        .chat_group_command(&format!("add agent:{own}:{first}"))
        .unwrap();
    state.conversation.send_to_group = true;
    state
        .conversation
        .composer
        .set_text("Keep this unsent draft");
    state.conversation.composer.left();
    state.conversation.target = Some("example-reviewer".into());
    state.conversation.native_recipient = Some(format!("agent:{own}:{second}"));
    state.conversation.native_account = Some(("github".into(), "42".into()));
    let before = state.chat_saved_draft();
    let group = state.conversation.snapshot()["group_selected"].clone();

    for agent in [&first, &second] {
        state.chat_change_view(View::Agents);
        state.conversation.selected_agent = Some(format!("native:agent:{own}:{agent}"));
        state.conversation.content_focus = true;
        state.conversation.detail = false;
        let frame = screen_contents(&state.chat_frame(Instant::now()));
        assert!(
            frame.contains("h / right-click · read-only history"),
            "{frame}"
        );
        state.chat_character('h');
        assert_eq!(state.conversation.view, View::History);
        let snapshot = state.conversation.snapshot();
        assert_eq!(snapshot["thread_history"]["scope"]["client_id"], own);
        assert_eq!(
            snapshot["thread_history"]["scope"]["agent_id"],
            agent.as_str()
        );
        assert_eq!(snapshot["thread_history"]["scope"]["kind"], "thread");
        assert_eq!(
            snapshot["thread_history"]["scope"]["provenance"],
            "authenticated_call_scope_or_local_input_lease"
        );
        assert_eq!(snapshot["thread_history"]["read_only"], true);
        assert_eq!(state.chat_saved_draft(), before);
        assert_eq!(snapshot["group_selected"], group);
        assert!(state.conversation.submitting.is_none());
        assert!(state.conversation.native_pending.is_none());
        assert!(state.actions.is_empty());
        state.chat_key(Key::Escape);
        assert_eq!(state.conversation.view, View::Agents);
    }

    state.chat_change_view(View::Activity);
    state.conversation.content_focus = false;
    state.conversation.composer.set_text("");
    state.chat_character('h');
    assert_eq!(state.conversation.view, View::Activity);
    assert_eq!(state.conversation.composer.text(), "h");
    state.chat_change_view(View::Agents);
    state.conversation.content_focus = true;
    assert!(
        state
            .chat_keyboard(b"\x1b[200~h\x1b[201~", Instant::now(), false)
            .is_empty()
    );
    assert_eq!(state.conversation.view, View::Activity);
    assert_eq!(state.conversation.composer.text(), "hh");
}

fn sidebar_state() -> State {
    let mut state = preview_state(
        32,
        160,
        &Ui {
            sidebar: true,
            panels: vec![crate::client::ui::Panel::Agents],
            ..Ui::default()
        },
    );
    state.audit = Default::default();
    state.conversation.composer.set_text("Keep sidebar draft");
    state.conversation.composer.left();
    state.conversation.target = Some("example-reviewer".into());
    state.conversation.native_recipient = Some(format!("client:{}", "8".repeat(64)));
    state.conversation.native_account = Some(("github".into(), "42".into()));
    state
}

fn caller(state: &mut State, run: &str, name: &str) {
    state.audit.ingest(&json!({
        "type":"ssmcp.code_mode","phase":"started","run_id":run,
        "controller":{"name":name,"model":"Model","source":"caller-reported"}
    }));
}

#[test]
fn sidebar_mouse_uses_painted_identity_and_the_exact_drawn_call() {
    let mut state = sidebar_state();
    caller(&mut state, "drawn", "Original");
    state.chat_frame(Instant::now());
    let hit = state
        .conversation
        .sidebar_agents
        .iter()
        .find(|hit| hit.identity.run_id.as_deref() == Some("drawn"))
        .unwrap()
        .clone();
    let before = state.chat_saved_draft();
    caller(&mut state, "reordered", "Another caller");
    state.chat_key(Key::Mouse {
        button: 0,
        x: hit.rect.col + 2,
        y: hit.row,
        release: false,
    });
    assert_eq!(state.conversation.view, View::Agents);
    assert_eq!(
        state.conversation.selected_agent.as_ref(),
        Some(&hit.identity.key)
    );
    assert!(!state.conversation.detail);
    assert_eq!(state.chat_saved_draft(), before);

    state.chat_change_view(View::Activity);
    state.chat_frame(Instant::now());
    let hit = state
        .conversation
        .sidebar_agents
        .iter()
        .find(|hit| hit.identity.run_id.as_deref() == Some("drawn"))
        .unwrap()
        .clone();
    caller(&mut state, "newer-in-same-group", "Original");
    state.chat_key(Key::Mouse {
        button: 2,
        x: hit.rect.col + 2,
        y: hit.row,
        release: false,
    });
    assert_eq!(state.conversation.view, View::Agents);
    assert!(state.conversation.detail);
    assert_eq!(state.audit.snapshot()["selected"]["run_id"], "drawn");
    assert_eq!(state.chat_saved_draft(), before);
    assert!(state.actions.is_empty());

    state.chat_key(Key::Escape);
    assert_eq!(state.conversation.view, View::Agents);
    assert!(!state.conversation.detail);
    let recipient_column = state.conversation.snapshot()["canvas"]["active"]
        .as_u64()
        .unwrap();
    state.chat_key(Key::Open);
    assert_eq!(
        state.conversation.snapshot()["canvas"]["active"],
        recipient_column + 1
    );
    assert_eq!(
        state.conversation.view,
        View::Audit,
        "right arrow reaches the panel after the explicitly opened recipients"
    );
    assert_eq!(state.chat_saved_draft(), before);
}

#[test]
fn sidebar_expired_member_or_call_never_falls_back_to_a_replacement_or_logs() {
    let mut state = sidebar_state();
    state.chat_frame(Instant::now());
    // The old painted member left while another member still occupies this row.
    let hit = state
        .conversation
        .sidebar_agents
        .iter_mut()
        .find(|hit| hit.identity.key == "member:example-builder")
        .unwrap();
    hit.identity.key = "member:expired".into();
    let hit = hit.clone();
    let before = state.chat_saved_draft();
    for button in [0, 2] {
        state.chat_key(Key::Mouse {
            button,
            x: hit.rect.col + 2,
            y: hit.row,
            release: false,
        });
        assert_eq!(state.conversation.view, View::Activity);
        assert!(
            state
                .notice
                .as_deref()
                .unwrap()
                .contains("no longer available")
        );
        assert_eq!(state.chat_saved_draft(), before);
    }

    caller(&mut state, "evicted", "Same caller");
    state.chat_frame(Instant::now());
    let hit = state
        .conversation
        .sidebar_agents
        .iter()
        .find(|hit| hit.identity.run_id.as_deref() == Some("evicted"))
        .unwrap()
        .clone();
    state.audit = Default::default();
    caller(&mut state, "replacement", "Same caller");
    state.chat_key(Key::Mouse {
        button: 2,
        x: hit.rect.col + 2,
        y: hit.row,
        release: false,
    });
    assert_eq!(state.conversation.view, View::Agents);
    assert!(!state.conversation.detail);
    assert!(
        state
            .notice
            .as_deref()
            .unwrap()
            .contains("no longer in the retained")
    );
    assert_eq!(state.chat_saved_draft(), before);
    assert!(state.actions.is_empty());
}

#[test]
fn sidebar_hits_exist_only_for_visible_rows_in_the_current_frame() {
    let mut state = sidebar_state();
    state.chat_frame(Instant::now());
    let hit = state.conversation.sidebar_agents.first().unwrap().clone();
    let before = state.chat_saved_draft();
    assert!(
        !state.chat_sidebar_agent_mouse(0, hit.rect.col + 2, 0),
        "heading is not a member"
    );
    assert!(
        !state.chat_sidebar_agent_mouse(0, hit.rect.col + 2, state.geometry.rows - 1),
        "footer has no hit"
    );

    state.conversation.process_generation += 1;
    assert!(state.chat_sidebar_agent_mouse(2, hit.rect.col + 2, hit.row));
    assert_eq!(state.conversation.view, View::Activity);
    assert!(
        state
            .notice
            .as_deref()
            .unwrap()
            .contains("Recipients changed")
    );
    state.conversation.process_generation -= 1;
    state.help = true;
    assert!(!state.chat_sidebar_agent_mouse(0, hit.rect.col + 2, hit.row));
    state.help = false;
    state.sleep.visible = true;
    assert!(!state.chat_sidebar_agent_mouse(0, hit.rect.col + 2, hit.row));
    state.sleep.visible = false;
    state.ui.sidebar = false;
    assert!(!state.chat_sidebar_agent_mouse(0, hit.rect.col + 2, hit.row));
    state.chat_frame(Instant::now());
    assert!(state.conversation.sidebar_agents.is_empty());

    state.ui.sidebar = true;
    state.chat_frame(Instant::now());
    state.resize(32, 180);
    assert!(!state.chat_sidebar_agent_mouse(0, hit.rect.col + 2, hit.row));
    state.resize(10, 70);
    state.chat_frame(Instant::now());
    assert!(state.conversation.sidebar_agents.is_empty());
    assert_eq!(state.chat_saved_draft(), before);
    assert!(state.actions.is_empty());
}
