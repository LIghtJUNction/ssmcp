use super::*;

#[test]
fn concise_renderer_keeps_full_redacted_command_in_expandable_view() {
    let script = format!(
        "print('first')\nAPI_TOKEN=private-command-secret\n{}\nprint('last-marker')",
        "界".repeat(700)
    );
    let event = json!({"type":"exec.started","job_id":"command-full",
        "command":["python3","-c",script],"cwd":"/fixture"});
    let original = event.clone();
    let bytes = crate::client::render::render_event_for_test("shell", &event, 48).unwrap();
    let concise = String::from_utf8(bytes.clone()).unwrap();
    assert!(!concise.contains("last-marker"));
    let mut view = EventView::new();
    view.ingest_event("shell", &event, &bytes);
    view.set_scope(EventScope::Conversation);
    let folded = view
        .render(
            48,
            24,
            &Ui {
                color: false,
                ..Ui::default()
            },
            true,
        )
        .join("\n");
    assert!(!folded.contains("last-marker"));
    assert!(view.open());
    let retained = &view.events[0].text;
    assert!(retained.contains("last-marker") && retained.contains(&"界".repeat(700)));
    assert!(retained.contains("[redacted]"));
    assert!(!retained.contains("private-command-secret"));
    assert_eq!(event, original);
    for width in [48, 120] {
        let _ = view.render(
            width,
            24,
            &Ui {
                color: false,
                ..Ui::default()
            },
            true,
        );
        view.scroll_selected_panel(isize::MAX);
        let tail = view
            .render(
                width,
                24,
                &Ui {
                    color: false,
                    ..Ui::default()
                },
                true,
            )
            .join("\n");
        assert!(tail.contains("last-marker"), "{tail}");
    }
}

#[test]
fn unrelated_event_fields_cannot_add_a_command_source() {
    let event = json!({"type":"ssmcp.message","text":"ordinary message",
        "command":["should-not-be-command"]});
    assert!(crate::client::render::interactive_command_source(&event).is_none());
}
