use super::*;

fn ui() -> Ui {
    Ui {
        color: false,
        ..Ui::default()
    }
}
fn fixture() -> EventView {
    let mut view = EventView::new();
    view.set_scope(EventScope::Conversation);
    view.ingest_event(
        "message",
        &json!({"type":"ssmcp.message","role":"user","text":"Keep this conversation."}),
        b"",
    );
    for id in ["old-job-a", "old-job-b"] {
        let source = format!(
            "print('head-marker')\nAPI_TOKEN=private-reload-source\n{}\nprint('tail-marker')",
            "界".repeat(700)
        );
        view.ingest_event("shell",&json!({"type":"exec.started","job_id":id,"request_id":"old-request","command":["python3","-c",source]}),
            b"[RUN] python3 -c started\n");
    }
    view.ingest_event(
        "shell",
        &json!({"type":"exec.exited","job_id":"old-job-b","exit_code":0}),
        b"[OK] exit=0\n",
    );
    view
}

#[test]
fn reload_display_restores_new_inert_ids_and_bounded_reading_positions() {
    let mut before = fixture();
    let old_ids = before
        .events
        .iter()
        .map(|event| event.id)
        .collect::<Vec<_>>();
    assert!(
        before.events[1]
            .meta
            .keys
            .iter()
            .any(|key| key.contains("old-job-a"))
    );
    assert!(before.open());
    before.select(2);
    assert!(before.open());
    before.render(48, 24, &ui(), true);
    assert!(before.scroll_selected_panel(6));
    before.render(48, 24, &ui(), true);
    let position = before.position().for_reload(&before);
    let serialized = serde_json::to_vec(&before.reload_display()).unwrap();
    let text = String::from_utf8(serialized.clone()).unwrap();
    assert!(!text.contains("private-reload-source"));
    assert!(
        !text.contains("old-job-a") && !text.contains("old-job-b") && !text.contains("old-request")
    );
    let display: Snapshot = serde_json::from_slice(&serialized).unwrap();
    let position: ReloadPosition =
        serde_json::from_slice(&serde_json::to_vec(&position).unwrap()).unwrap();
    let mut after = EventView::new();
    after.set_scope(EventScope::Conversation);
    assert!(after.restore_reload_display(display).is_none());
    let position = position.position(&after);
    after.restore_position(&position);
    after.render(48, 24, &ui(), true);
    assert!(
        after
            .events
            .iter()
            .all(|event| !old_ids.contains(&event.id))
    );
    assert!(after.events.iter().all(|event| event.meta.keys.is_empty()
        && event.meta.scope.is_none()
        && event.meta.read.is_none()
        && !event.meta.started));
    assert_eq!(after.events[1].meta.status, None);
    assert!(after.events[1].historical && after.events[1].interrupted);
    assert_eq!(after.events[2].meta.status, Some(Status::Exit(0)));
    assert_eq!(after.events[2].panel.offset(), 6);
    assert!(after.events[2].chat_expanded && after.tool_groups()[0].expanded);
    assert_eq!(after.selected, Some(after.events[2].id));
    assert_eq!(after.snapshot()["tool_groups"][0]["stats"]["running"], 0);
    assert_eq!(after.snapshot()["tool_groups"][0]["stats"]["unknown"], 1);
    let old_text = after.events[1].text.clone();
    after.ingest_event(
        "shell",
        &json!({"type":"exec.stdout","job_id":"old-job-a"}),
        b"new connection output\n",
    );
    assert_eq!(
        after.events.len(),
        4,
        "native updates cannot coalesce into the historical card"
    );
    assert_eq!(after.events[1].text, old_text);
}

#[test]
fn reload_display_keeps_both_outer_fold_states_and_separates_new_live_calls() {
    for expanded in [false, true] {
        let mut before = fixture();
        if expanded {
            assert!(before.open());
        }
        before.render(120, 24, &ui(), true);
        let position = before.position().for_reload(&before);
        let mut after = EventView::new();
        after.set_scope(EventScope::Conversation);
        after.restore_reload_display(before.reload_display());
        let restored = position.position(&after);
        after.restore_position(&restored);
        after.render(120, 24, &ui(), true);
        assert_eq!(after.tool_groups()[0].expanded, expanded);
        assert_eq!(after.selected_group, Some(after.tool_groups()[0].head));
        for id in ["new-a", "new-b"] {
            after.ingest_event(
                "shell",
                &json!({"type":"exec.started","job_id":id}),
                b"[RUN] next command\n",
            );
        }
        assert_eq!(after.tool_groups().len(), 2);
        assert_eq!(after.tool_groups()[0].members.len(), 2);
        assert_eq!(after.tool_groups()[1].members.len(), 2);
    }
}

#[test]
fn reload_display_budget_counts_serialized_bytes_and_retains_safe_tails() {
    let seed = fixture().reload_display().entries[1].clone();
    let mut entries = Vec::new();
    for index in 0..24 {
        let mut entry = seed.clone();
        entry.index = index;
        entry.text = format!("first\n{}\nlast\n", "\"\n".repeat(MAX_DETAIL / 5));
        entry.source = Some(Source {
            text: format!(
                "head-source\nAPI_TOKEN=private-handoff-token\n{}\ntail-source",
                "界".repeat(MAX_DETAIL)
            ),
            truncated: false,
        });
        entries.push(entry);
    }
    let mut snapshot = Snapshot {
        version: 1,
        entries,
        omitted: false,
    };
    snapshot.bound();
    let bytes = serde_json::to_vec(&snapshot).unwrap();
    assert!(bytes.len() <= MAX_BYTES);
    assert!(snapshot.omitted && snapshot.entries.len() < 24);
    assert!(
        snapshot
            .entries
            .iter()
            .all(|entry| entry.text.len() <= MAX_DETAIL)
    );
    for entry in &snapshot.entries {
        let source = entry.source.as_ref().unwrap();
        assert!(source.truncated);
        assert!(source.text.contains("head-source") && source.text.contains("tail-source"));
        assert!(!source.text.contains("private-handoff-token"));
    }
    let mut restored = EventView::new();
    let notice = restored.restore_reload_display(snapshot).unwrap();
    assert!(notice.contains("shortened"));
    assert!(restored.events.len() <= MAX_EVENTS && restored.bytes <= MAX_BYTES);

    let entries = (0..MAX_EVENTS + 30)
        .map(|index| Entry {
            index: index % MAX_EVENTS,
            ..seed.clone()
        })
        .collect();
    let mut many = Snapshot {
        version: 1,
        entries,
        omitted: false,
    };
    many.bound();
    assert!(many.omitted && many.entries.len() <= MAX_EVENTS);
}

#[test]
fn reload_display_invalid_preview_and_old_positions_do_not_discard_conversation() {
    let mut view = fixture();
    let before = view
        .events
        .iter()
        .map(|event| event.text.clone())
        .collect::<Vec<_>>();
    let bad: Snapshot =
        serde_json::from_value(json!({"version":1,"entries":"bad preview","omitted":false}))
            .unwrap();
    assert!(view.restore_reload_display(bad).is_some());
    assert_eq!(
        view.events
            .iter()
            .map(|event| event.text.clone())
            .collect::<Vec<_>>(),
        before
    );
    let old: ReloadPosition = serde_json::from_value(json!({"scroll":7,"follow":false})).unwrap();
    let position = old.position(&view);
    assert_eq!(position.scroll, 7);
    assert!(!position.follow && position.panel_scroll.is_empty());
    let bad: ReloadPosition =
        serde_json::from_value(json!({"scroll":3,"follow":true,"reading":{"panels":"invalid"}}))
            .unwrap();
    assert!(bad.position(&view).panel_scroll.is_empty());
}
