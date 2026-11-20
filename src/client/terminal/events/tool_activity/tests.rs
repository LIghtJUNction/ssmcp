use super::super::{EventScope, EventView, MAX_EVENTS, Status, Ui, unstyled};
use serde_json::json;
use unicode_width::UnicodeWidthStr;

fn ui() -> Ui {
    Ui {
        color: false,
        ..Ui::default()
    }
}
fn view() -> EventView {
    let mut view = EventView::new();
    view.set_scope(EventScope::Conversation);
    view
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
fn tool(view: &mut EventView, id: &str, output: usize) -> u64 {
    view.ingest_event(
        "native",
        &json!({"type":"harness.tool.started","call_id":id}),
        format!("[REQ] inspect {id} call={id} started\n").as_bytes(),
    );
    let event_id = view.events.back().unwrap().id;
    if output > 0 {
        view.ingest_event(
            "native",
            &json!({"type":"tool_result","call_id":id}),
            format!(
                "[RES] native\n{}",
                (0..output)
                    .map(|n| format!("{id}-{n:02}\n"))
                    .collect::<String>()
            )
            .as_bytes(),
        );
    }
    event_id
}
fn open_group(view: &mut EventView) {
    let group = view.tool_groups().last().unwrap().clone();
    view.selected = Some(group.head);
    view.selected_group = Some(group.head);
    assert!(view.open());
}
fn image_preview() -> super::super::images::Preview {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    let image = image::DynamicImage::ImageRgba8(image::RgbaImage::new(2, 2));
    let mut png = std::io::Cursor::new(Vec::new());
    image.write_to(&mut png, image::ImageFormat::Png).unwrap();
    super::super::images::decode(
        &json!({"type":"image","mimeType":"image/png","data":STANDARD.encode(png.into_inner())}),
        false,
    )
    .unwrap()
}

#[test]
fn adjacent_tool_groups_default_to_summary_and_count_logical_calls_once() {
    for width in [120, 48] {
        let mut view = view();
        tool(&mut view, "running", 20);
        tool(&mut view, "passed", 20);
        tool(&mut view, "failed", 20);
        view.ingest_event(
            "native",
            &json!({"type":"harness.tool.completed","call_id":"passed","exit_code":0}),
            b"[OK] shell passed exit=0\n",
        );
        view.ingest_event(
            "native",
            &json!({"type":"harness.tool.completed","call_id":"failed","exit_code":1}),
            b"[ERR] shell failed exit=1\n",
        );
        view.ingest_event(
            "workspace",
            &json!({"type":"ssmcp.file.read","path":"a.rs"}),
            b"[READ] workspace a.rs\n",
        );
        for _ in 0..5 {
            view.ingest_event(
                "native",
                &json!({"type":"tool_result","call_id":"running"}),
                b"more output\n",
            );
        }
        let text = view.render(width, 24, &ui(), true).join("\n");
        let snapshot = view.snapshot();
        assert_eq!(snapshot["tool_groups"][0]["count"], 4);
        assert_eq!(
            snapshot["tool_groups"][0]["stats"],
            json!({"running":1,"done":1,"failed":1,"unknown":1})
        );
        assert_eq!(snapshot["tool_groups"][0]["expanded"], false);
        assert_eq!(snapshot["visible_cards"], 1);
        assert!(
            text.contains("Tools · 4 calls") && text.contains("unknown 1"),
            "{text}"
        );
        assert!(!text.contains("more output") && !text.contains("running-19"));
        assert!(view.total_rows <= 2);
        assert_eq!(view.visible_ids().len(), 1);
        assert!(view.open());
        view.render(width, 24, &ui(), true);
        assert!(view.total_rows > 2);
        assert_eq!(view.snapshot()["visible_cards"], 4);
        assert!(view.mouse_select(0));
        assert!(view.toggle());
        view.render(width, 24, &ui(), true);
        assert!(view.total_rows <= 2);
        assert_eq!(view.events.len(), 4);
    }
}

#[test]
fn adjacent_tool_windows_respect_conversation_image_and_diagnostic_boundaries() {
    let mut view = view();
    tool(&mut view, "a", 0);
    tool(&mut view, "b", 0);
    view.ingest_conversation(b"[AGENT] Next checks\n");
    tool(&mut view, "c", 0);
    tool(&mut view, "d", 0);
    view.ingest(b"diagnostic boundary\n");
    tool(&mut view, "e", 0);
    tool(&mut view, "f", 0);
    assert_eq!(view.tool_groups().len(), 3);
    view.events[1].image = Some(image_preview());
    assert_eq!(view.tool_groups().len(), 2);
    view.events[3].meta.diff = true;
    assert_eq!(view.tool_groups().len(), 2, "diffs are logical tool calls");
    view.filter("cargo", 0);
    assert!(
        view.tool_groups().is_empty(),
        "search must show exact matching records"
    );
    view.filter("", 1);
    assert!(view.tool_groups().is_empty());
    view.filter("", 0);
    view.set_scope(EventScope::All);
    assert!(view.tool_groups().is_empty(), "raw logs are not grouped");
}

#[test]
fn adjacent_tool_windows_count_read_groups_and_ignore_bookkeeping() {
    let mut view = view();
    tool(&mut view, "before-a", 0);
    for name in ["one", "two", "three"] {
        view.ingest_event(
            "workspace",
            &json!({"type":"ssmcp.file.read","path":format!("skills/{name}/SKILL.md")}),
            b"[READ] workspace skill\n",
        );
        view.ingest_event("audit",&json!({"type":"ssmcp.skill.discovered","tool":"ssmcp.skills.list","path":name,"invoked":false}),b"[SKIL] discovered\n");
    }
    tool(&mut view, "after-a", 0);
    assert_eq!(view.read_groups().len(), 1);
    assert_eq!(view.tool_groups().len(), 1);
    let snapshot = view.snapshot();
    assert_eq!(snapshot["tool_groups"][0]["count"], 5);
    assert_eq!(snapshot["tool_groups"][0]["stats"]["unknown"], 3);
    assert_eq!(snapshot["retained_events"], 8);
    let text = view.render(120, 24, &ui(), true).join("\n");
    assert!(
        text.contains("Tools · 5 calls") && !text.contains("discovered"),
        "{text}"
    );
}

#[test]
fn adjacent_tool_window_scroll_and_selection_keep_exact_event_targets() {
    for width in [120, 48, 12, 1] {
        let mut view = view();
        let ids = (0..8)
            .map(|n| tool(&mut view, &format!("job{n}"), 20))
            .collect::<Vec<_>>();
        let original = view
            .events
            .iter()
            .map(|event| event.text.clone())
            .collect::<Vec<_>>();
        open_group(&mut view);
        view.render(width, 24, &ui(), true);
        assert!(view.scroll_selected_panel(isize::MAX));
        let rows = view.render(width, 24, &ui(), true);
        assert!(view.total_rows <= 15);
        assert!(
            rows.iter()
                .all(|row| UnicodeWidthStr::width(unstyled(row).as_str()) <= usize::from(width))
        );
        assert!(view.events[0].group_panel.offset() > 0);
        let panel_offsets = view
            .events
            .iter()
            .map(|event| event.panel.offset())
            .collect::<Vec<_>>();
        let outer = view.scroll;
        assert!(view.scroll_panel_at(0, isize::MIN + 1));
        view.render(width, 24, &ui(), true);
        assert_eq!(view.events[0].group_panel.offset(), 0);
        assert_eq!(view.scroll, outer);
        let first = view
            .hit_rows
            .iter()
            .enumerate()
            .position(|(row, id)| *id == Some(ids[0]) && view.group_header_hit_rows[row].is_none())
            .unwrap();
        assert!(view.mouse_select(first as u16));
        assert_eq!(view.selected, Some(ids[0]));
        assert!(view.selected_group.is_none());
        assert!(view.scroll_selected_panel(3));
        view.render(width, 24, &ui(), true);
        assert_eq!(view.events[0].group_panel.offset(), 3);
        assert_eq!(
            view.events
                .iter()
                .map(|event| event.panel.offset())
                .collect::<Vec<_>>(),
            panel_offsets
        );
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
    }
}

#[test]
fn adjacent_commands_preserve_fold_inner_scroll_and_canvas_positions() {
    for width in [120, 48] {
        let mut view = view();
        let first = command(&mut view, "first", 40);
        let second = command(&mut view, "second", 40);
        view.render(width, 24, &ui(), true);
        assert!(view.tool_groups().is_empty());
        assert_eq!(view.visible_ids(), vec![first, second]);
        view.selected = Some(first);
        view.selected_group = None;
        view.follow = false;
        view.reveal = true;
        let folded = view.position();
        assert!(view.open());
        view.render(width, 24, &ui(), true);
        assert_eq!(view.events[0].panel.offset(), 0);
        let other_offset = view.events[1].panel.offset();
        assert!(view.scroll_selected_panel(3));
        view.render(width, 24, &ui(), true);
        assert_eq!(view.events[0].panel.offset(), 3);
        assert_eq!(view.events[1].panel.offset(), other_offset);
        let row = view
            .panel_hit_rows
            .iter()
            .position(|id| *id == Some(first))
            .unwrap();
        assert!(view.scroll_panel_at(row as u16, 3));
        let expanded = view.position();
        assert!(view.toggle());
        view.render(width, 24, &ui(), true);
        assert!(!view.events[0].chat_expanded);
        assert_eq!(view.events[0].panel.offset(), 37);
        view.restore_position(&expanded);
        view.render(width, 24, &ui(), true);
        assert!(view.events[0].chat_expanded);
        assert_eq!(view.events[0].panel.offset(), 6);
        assert_eq!(view.selected, Some(first));
        assert!(view.selected_group.is_none());
        view.restore_position(&folded);
        view.render(width, 24, &ui(), true);
        assert!(!view.events[0].chat_expanded);
        assert_eq!(view.events[0].panel.offset(), 37);
        assert_eq!(view.events[1].panel.offset(), other_offset);
    }
}

#[test]
fn standalone_command_details_reveal_long_redacted_source_tail_at_all_widths() {
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
        let transcript =
            crate::client::render::render_event_for_test("shell", &event, usize::from(width))
                .unwrap();
        view.ingest_event("shell", &event, &transcript);
        assert!(view.tool_groups().is_empty());
        view.selected = Some(view.events[1].id);
        view.selected_group = None;
        view.follow = false;
        view.reveal = true;
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
        tool(&mut view, &format!("job{index}"), 0);
        view.render(48, 20, &ui(), false);
    }
    assert_eq!(view.events.len(), MAX_EVENTS);
    assert_eq!(view.dropped, 30);
    assert_eq!(view.position().panel_scroll.len(), MAX_EVENTS);
    assert_eq!(view.snapshot()["tool_groups"][0]["count"], MAX_EVENTS);
    assert!(view.total_rows <= 2);
    open_group(&mut view);
    view.render(48, 20, &ui(), true);
    assert_eq!(view.snapshot()["tool_groups"][0]["visible_rows"], 12);
    assert!(view.total_rows <= 15);
    view.clear();
    assert!(view.tool_groups().is_empty() && view.group_hit_rows.is_empty());
}

#[test]
fn adjacent_commands_remain_visible_without_expanding_a_tool_group() {
    for width in [120, 48] {
        let mut view = view();
        let first = command(&mut view, "check", 1);
        let second = command(&mut view, "status", 0);
        view.ingest_event(
            "shell",
            &json!({"type":"exec.exited","job_id":"check","exit_code":0}),
            b"[OK] shell check exit=0\n",
        );
        let rows = view.render(width, 24, &ui(), false);
        let text = rows.join("\n");
        assert!(view.tool_groups().is_empty());
        assert_eq!(view.visible_ids(), vec![first, second]);
        assert!(
            text.contains("cargo check") && text.contains("cargo status"),
            "{text}"
        );
        assert!(
            text.contains("check-00") && text.contains("Show details"),
            "{text}"
        );
        assert!(!text.contains("Tools ·"), "{text}");
        assert!(
            rows.iter().all(|row| {
                UnicodeWidthStr::width(unstyled(row).as_str()) <= usize::from(width)
            })
        );
        view.selected = Some(second);
        view.selected_group = None;
        assert!(view.open());
        view.render(width, 24, &ui(), true);
        assert!(!view.events[0].chat_expanded);
        assert!(view.events[1].chat_expanded);
        assert_eq!(view.events[0].meta.status, Some(Status::Exit(0)));
        assert_eq!(view.events[1].meta.status, Some(Status::Running));
    }
}
