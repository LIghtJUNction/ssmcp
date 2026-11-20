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
    before.select(isize::MIN);
    before.select(1);
    assert!(before.back());
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
    assert!(
        after
            .events
            .iter()
            .all(|event| event.meta.status != Some(Status::Running))
    );
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
        before.select(isize::MIN);
        before.select(1);
        assert!(before.back());
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
fn command_group_reload_keeps_automatic_and_explicit_choices_and_legacy_bools() {
    for (choice, legacy) in [
        (None, false),
        (Some(false), false),
        (Some(true), false),
        (Some(false), true),
        (Some(true), true),
    ] {
        let mut before = fixture();
        if let Some(expanded) = choice {
            let group = before.tool_groups()[0].clone();
            assert!(before.set_tool_expanded(&group, expanded));
        }
        let display = serde_json::to_value(before.reload_display()).unwrap();
        let mut reading = serde_json::to_value(before.position().for_reload(&before)).unwrap();
        if legacy {
            for panel in reading["reading"]["panels"].as_array_mut().unwrap() {
                assert!(
                    panel[1]
                        .as_object_mut()
                        .unwrap()
                        .remove("group_automatic")
                        .is_some()
                );
            }
        }
        let reading: ReloadPosition = serde_json::from_value(reading).unwrap();
        let mut after = EventView::new();
        after.set_scope(EventScope::Conversation);
        after.restore_reload_display(serde_json::from_value(display).unwrap());
        after.restore_position(&reading.position(&after));
        let group = after.tool_groups()[0].clone();
        assert_eq!(group.expanded, choice.unwrap_or(true));
        assert_eq!(after.events[group.start].group_automatic, choice.is_none());
        for width in [120, 48] {
            let rows = after.render(width, 24, &ui(), false);
            let text = rows.join("\n");
            assert_eq!(
                text.contains("python3 -c"),
                choice.unwrap_or(true),
                "{text}"
            );
            assert!(!text.contains("private-reload-source"), "{text}");
        }
        assert!(
            after
                .events
                .iter()
                .all(|event| event.meta.scope.is_none() && event.meta.keys.is_empty())
        );
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
        entries,
        ..Snapshot::default()
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
        entries,
        ..Snapshot::default()
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

#[test]
fn reload_without_valid_owner_metadata_stays_out_of_the_selected_conversation() {
    let mut saved = serde_json::to_value(fixture().reload_display()).unwrap();
    assert!(saved.get("selected_history_id").is_none());
    saved["selected_history_id"] = json!("legacy_attach");
    saved["entries"][0]
        .as_object_mut()
        .unwrap()
        .remove("history");
    saved["entries"][1]["history"] = json!({"local":"bad-history-id"});
    let mut restored = EventView::new();
    restored.set_history_owner(HistoryId::parse(
        "conversation_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
    ));
    let saved: Snapshot = serde_json::from_value(saved).unwrap();
    assert!(saved.selected_history().is_none());
    restored.restore_reload_display(saved);
    assert!(restored.visible_ids().is_empty());
    assert_eq!(restored.events[0].meta.history, DisplayHistory::Global);
    assert_eq!(restored.events[1].meta.history, DisplayHistory::Global);
    restored.set_scope(EventScope::All);
    assert_eq!(restored.visible_ids().len(), 3);
}

#[test]
fn reload_display_keeps_transparent_read_bookkeeping_and_explicit_fold_before_first_frame() {
    let mut before = EventView::new();
    before.set_scope(EventScope::Conversation);
    for name in ["one", "two", "three"] {
        before.ingest_event(
            "workspace",
            &json!({"type":"ssmcp.file.read","path":format!("skills/{name}/SKILL.md")}),
            b"[READ] workspace clipped path\n",
        );
        before.ingest_event("audit",&json!({"type":"ssmcp.skill.discovered","tool":"ssmcp.skills.list","path":name,"invoked":false}),b"[SKIL] discovered\n");
    }
    assert_eq!(before.tool_groups()[0].members.len(), 3);
    assert!(before.open());
    before.render(48, 24, &ui(), true);
    assert!(before.back());
    before.render(48, 24, &ui(), true);
    assert!(!before.follow && before.selected_group.is_some());
    let position = before.position().for_reload(&before);
    let display: Snapshot =
        serde_json::from_slice(&serde_json::to_vec(&before.reload_display()).unwrap()).unwrap();
    // A newly allocated terminal has not rendered a conversation frame yet.
    let mut after = EventView::new();
    assert_eq!(after.scope, EventScope::All);
    after.restore_reload_display(display);
    let position = position.position(&after);
    after.restore_position(&position);
    after.render(48, 24, &ui(), true);
    assert_eq!(after.tool_groups().len(), 1);
    assert_eq!(after.tool_groups()[0].members.len(), 3);
    assert!(!after.tool_groups()[0].expanded);
    assert_eq!(after.selected_group, Some(after.tool_groups()[0].head));
    assert!(
        after
            .events
            .iter()
            .filter(|event| event.meta.kind == Kind::Log)
            .all(|event| event.meta.read_bookkeeping)
    );
    assert!(
        after
            .events
            .iter()
            .all(|event| event.meta.scope.is_none() && event.meta.keys.is_empty())
    );
}
