use super::super::{EventScope, EventView, MAX_EVENTS, Status, Ui, unstyled};
use crate::client::render::Renderer;
use serde_json::json;
use unicode_width::UnicodeWidthStr;

fn ui() -> Ui {
    Ui {
        color: false,
        ..Ui::default()
    }
}

fn command(view: &mut EventView, id: &str, output: usize) -> u64 {
    view.ingest_event(
        "shell",
        &json!({"type":"exec.started","job_id":id,"command":["cargo",id]}),
        format!("[RUN] shell {id} cargo {id} started\n").as_bytes(),
    );
    let event_id = view.events.back().unwrap().id;
    if output > 0 {
        view.ingest_event(
            "shell",
            &json!({"type":"exec.stdout","job_id":id}),
            (0..output)
                .map(|n| format!("{id}-{n:02}\n"))
                .collect::<String>()
                .as_bytes(),
        );
    }
    event_id
}

fn view() -> EventView {
    let mut view = EventView::new();
    view.set_scope(EventScope::Conversation);
    view
}

#[test]
fn adjacent_tool_windows_respect_prose_hidden_logs_and_diff_boundaries() {
    let mut view = view();
    command(&mut view, "a", 0);
    command(&mut view, "b", 0);
    view.ingest_conversation(b"[AGENT] Next checks\n");
    command(&mut view, "c", 0);
    command(&mut view, "d", 0);
    view.ingest(b"diagnostic boundary\n");
    command(&mut view, "e", 0);
    command(&mut view, "f", 0);
    assert_eq!(view.tool_groups().len(), 3);
    assert!(
        view.tool_groups()
            .iter()
            .all(|group| group.end - group.start == 2)
    );
    let text = view.render(120, 40, &ui(), false).join("\n");
    assert_eq!(text.matches("Tools · 2 calls").count(), 3);
    assert!(!text.contains("diagnostic boundary"));

    view.events[1].meta.diff = true;
    assert_eq!(view.tool_groups().len(), 2);
    view.filter("cargo", 0);
    assert!(
        view.tool_groups().is_empty(),
        "filtered rows must retain their original identities"
    );
    view.filter("", 1);
    assert!(view.tool_groups().is_empty());
    view.filter("", 0);
    view.set_scope(EventScope::All);
    assert!(view.tool_groups().is_empty(), "raw logs are never grouped");
}

#[test]
fn adjacent_tool_windows_keep_existing_read_groups_separate() {
    let mut view = view();
    command(&mut view, "before-a", 0);
    command(&mut view, "before-b", 0);
    for name in ["one", "two", "three"] {
        view.ingest_event(
            "workspace",
            &json!({"type":"ssmcp.file.read","path":format!("skills/{name}/SKILL.md")}),
            b"[READ] workspace skill\nretained detail\n",
        );
    }
    command(&mut view, "after-a", 0);
    command(&mut view, "after-b", 0);
    assert_eq!(view.read_groups().len(), 1);
    let groups = view.tool_groups();
    assert_eq!(groups.len(), 2);
    assert_eq!(groups[0].end, 2);
    assert_eq!(groups[1].start, 5);
    let text = view.render(120, 40, &ui(), false).join("\n");
    assert!(text.contains("Read 3 items"), "{text}");
    assert_eq!(text.matches("Tools · 2 calls").count(), 2);
}

#[test]
fn adjacent_tool_window_scroll_and_selection_keep_exact_event_targets() {
    for width in [120, 48, 12, 1] {
        let mut view = view();
        let ids = (0..8)
            .map(|n| command(&mut view, &format!("job{n}"), 20))
            .collect::<Vec<_>>();
        let original = view
            .events
            .iter()
            .map(|event| event.text.clone())
            .collect::<Vec<_>>();
        let rows = view.render(width, 24, &ui(), true);
        assert!(view.total_rows <= 14);
        assert!(
            rows.iter()
                .all(|row| UnicodeWidthStr::width(unstyled(row).as_str()) <= usize::from(width))
        );
        assert_eq!(view.events[0].group_panel.page, 12);
        assert!(view.events[0].group_panel.offset() > 0);
        let panel_offsets = view
            .events
            .iter()
            .map(|event| event.panel.offset())
            .collect::<Vec<_>>();
        let outer_scroll = view.scroll;
        assert!(view.scroll_panel_at(0, isize::MIN + 1));
        let rows = view.render(width, 24, &ui(), true);
        assert_eq!(view.events[0].group_panel.offset(), 0);
        assert_eq!(view.scroll, outer_scroll);
        assert_eq!(
            view.events
                .iter()
                .map(|event| event.panel.offset())
                .collect::<Vec<_>>(),
            panel_offsets
        );
        assert_eq!(view.hit_rows[1], Some(ids[0]));
        assert!(view.mouse_select(1));
        assert_eq!(view.selected, Some(ids[0]));
        assert!(view.scroll_selected_panel(3));
        view.render(width, 24, &ui(), true);
        assert_eq!(view.events[0].group_panel.offset(), 3);
        assert_eq!(view.events[0].panel.offset(), panel_offsets[0]);
        // Selecting a previously hidden member reveals its own title, not the head.
        view.select(7);
        view.render(width, 24, &ui(), true);
        assert_eq!(view.selected, Some(ids[7]));
        assert!(view.hit_rows.contains(&Some(ids[7])));
        assert_eq!(
            view.events
                .iter()
                .map(|event| event.text.clone())
                .collect::<Vec<_>>(),
            original
        );
        assert_eq!(rows.len(), 24);
    }
}

#[test]
fn adjacent_tool_windows_preserve_inner_scroll_and_canvas_positions() {
    let mut view = view();
    let ids = (0..6)
        .map(|n| command(&mut view, &format!("job{n}"), 40))
        .collect::<Vec<_>>();
    view.render(120, 24, &ui(), true);
    view.selected = Some(ids[2]);
    view.follow = false;
    view.reveal = true;
    assert!(view.open());
    view.render(120, 24, &ui(), true);
    let group_offset = view.events[0].group_panel.offset();
    assert!(view.scroll_selected_panel(3));
    view.render(120, 24, &ui(), true);
    assert_eq!(view.events[2].panel.offset(), 3);
    assert_eq!(view.events[0].group_panel.offset(), group_offset);
    let row = view
        .panel_hit_rows
        .iter()
        .position(|id| *id == Some(ids[2]))
        .unwrap();
    assert!(view.scroll_panel_at(row as u16, 3));
    assert_eq!(view.events[2].panel.offset(), 6);
    assert_eq!(view.events[0].group_panel.offset(), group_offset);
    let saved = view.position();
    assert!(view.scroll_tool_group(ids[2], isize::MAX));
    view.events[2].panel.scroll = Some(0);
    view.restore_position(&saved);
    view.render(120, 24, &ui(), true);
    assert_eq!(view.events[0].group_panel.offset(), group_offset);
    assert_eq!(view.events[2].panel.offset(), 6);
    assert!(view.toggle());
    view.render(120, 24, &ui(), true);
    assert_eq!(
        view.events[2].panel.offset(),
        37,
        "folding restores the previous output tail"
    );
    view.latest();
    view.render(120, 24, &ui(), true);
    assert!(view.events[0].group_panel.scroll.is_none());
}

#[test]
fn adjacent_tool_details_reveal_long_redacted_source_tail_at_all_widths() {
    for width in [120, 48] {
        let mut view = view();
        command(&mut view, "first", 3);
        let script = format!(
            "print('head-marker')\nAPI_TOKEN=private-group-secret\n{}\nprint('tail-marker')",
            "界".repeat(700)
        );
        let event =
            json!({"type":"exec.started","job_id":"long","command":["python3","-c",script]});
        let original = event.clone();
        let mut renderer = Renderer::new(false, false, usize::from(width));
        let mut transcript = Vec::new();
        renderer.event(&mut transcript, "shell", &event).unwrap();
        view.ingest_event("shell", &event, &transcript);
        view.render(width, 24, &ui(), true);
        assert!(view.open());
        let head = view.render(width, 24, &ui(), true).join("\n");
        assert!(head.contains("head-marker"), "{head}");
        assert!(view.scroll_selected_panel(isize::MAX));
        let tail = view.render(width, 24, &ui(), true).join("\n");
        assert!(tail.contains("tail-marker"), "{tail}");
        assert!(!head.contains("private-group-secret") && !tail.contains("private-group-secret"));
        let retained = view.events[1].command_source.as_ref().unwrap();
        assert!(
            retained.text.contains("[redacted]") && !retained.text.contains("private-group-secret")
        );
        assert_eq!(view.events[1].text.as_bytes(), transcript);
        assert_eq!(view.events[1].meta.status, Some(Status::Running));
        assert_eq!(event, original);
    }
}

#[test]
fn adjacent_tool_window_state_is_bounded_by_retained_events() {
    let mut view = view();
    for index in 0..MAX_EVENTS + 30 {
        command(&mut view, &format!("job{index}"), 0);
        view.render(48, 20, &ui(), false);
    }
    assert_eq!(view.events.len(), MAX_EVENTS);
    assert_eq!(view.dropped, 30);
    assert_eq!(view.position().panel_scroll.len(), MAX_EVENTS);
    let group = &view.snapshot()["tool_groups"][0];
    assert_eq!(group["count"], MAX_EVENTS);
    assert_eq!(group["members"].as_array().unwrap().len(), MAX_EVENTS);
    assert_eq!(group["visible_rows"], 12);
    assert_eq!(view.total_rows, 14);
    view.clear();
    assert!(view.tool_groups().is_empty() && view.group_hit_rows.is_empty());
}
