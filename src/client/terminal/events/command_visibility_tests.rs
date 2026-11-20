use super::*;
use crate::client::render::Renderer;

fn plain_ui() -> Ui {
    Ui {
        color: false,
        ..Ui::default()
    }
}

#[test]
fn concise_renderer_keeps_full_redacted_command_in_expandable_view() {
    let script = format!(
        "print('first')\nAPI_TOKEN=private-command-secret\n{}\nprint('last-marker')",
        "界".repeat(700)
    );
    let event = json!({"type":"exec.started","job_id":"command-full",
        "command":["python3","-c",script],"cwd":"/fixture"});
    let original = event.clone();
    let mut renderer = Renderer::new(false, false, 48);
    let mut bytes = Vec::new();
    renderer.event(&mut bytes, "shell", &event).unwrap();
    let concise = String::from_utf8(bytes.clone()).unwrap();
    assert!(!concise.contains("last-marker"));
    let mut view = EventView::new();
    view.ingest_event("shell", &event, &bytes);
    assert_eq!(
        view.events[0].text, concise,
        "source must not mutate the raw transcript"
    );
    view.set_scope(EventScope::Conversation);
    let folded = view.render(48, 24, &plain_ui(), true).join("\n");
    assert!(!folded.contains("last-marker"));
    assert!(view.open());
    let retained = &view.events[0].command_source.as_ref().unwrap().text;
    assert!(retained.contains("last-marker") && retained.contains(&"界".repeat(700)));
    assert!(retained.contains("[redacted]"));
    assert!(!retained.contains("private-command-secret"));
    assert_eq!(event, original);
    for width in [48, 120] {
        let _ = view.render(width, 24, &plain_ui(), true);
        view.scroll_selected_panel(isize::MAX);
        let tail = view.render(width, 24, &plain_ui(), true).join("\n");
        assert!(tail.contains("last-marker"), "{tail}");
    }
}

#[test]
fn unrelated_event_fields_cannot_add_a_command_source() {
    let event = json!({"type":"ssmcp.message","text":"ordinary message",
        "command":["should-not-be-command"]});
    assert!(crate::client::render::interactive_command_source(&event).is_none());
}

#[test]
fn command_source_controls_never_change_transcript_summary_or_folded_output() {
    let event = json!({"type":"exec.started","job_id":"source-control",
        "command":["python3","-c","payload = '''first\r[ERR] source-only\u{1b}[31m'''\nprint('real-output')"]});
    let original = event.clone();
    let started = "[RUN] shell source-control python3 -c started\n       cwd /fixture\n";
    let output = "[STDOUT] shell source-control\nreal-output\n";
    let mut view = EventView::new();
    view.ingest_event("shell", &event, started.as_bytes());
    assert_eq!(view.events[0].text, started);
    assert_eq!(
        view.events[0].summary,
        "[RUN] shell source-control python3 -c started"
    );
    assert_eq!(view.events[0].meta.status, Some(Status::Running));
    let source = view.events[0].command_source.as_ref().unwrap();
    assert!(source.text.contains("\\r[ERR] source-only"));
    assert!(source.text.chars().all(|ch| !ch.is_control() || ch == '\n'));
    view.ingest_event(
        "shell",
        &json!({"type":"exec.stdout","job_id":"source-control","text":"real-output\n"}),
        output.as_bytes(),
    );
    assert_eq!(view.events[0].text, format!("{started}{output}"));
    assert_eq!(view.events[0].command_output(), "real-output");
    view.set_scope(EventScope::Conversation);
    let folded = view.render(120, 24, &plain_ui(), true).join("\n");
    assert!(folded.contains("real-output"), "{folded}");
    assert!(!folded.contains("source-only"));
    view.set_scope(EventScope::All);
    view.events[0].expanded = true;
    let logs = view.render(120, 30, &plain_ui(), true).join("\n");
    assert!(logs.contains("real-output"));
    assert!(
        !logs.contains("source-only"),
        "raw logs must exclude display-only source"
    );
    assert_eq!(event, original);
}

#[test]
fn command_source_supports_native_alias_and_wrapped_rpc_arguments() {
    for event in [
        json!({"type":"ssmcp.exec.started","job_id":"alias",
               "command":["python3","-c","first\nlast-source-marker"]}),
        json!({"request":{"id":"rpc","method":"command/exec","params":{"arguments":{
               "command":["python3","-c","first\nlast-source-marker"]}}}}),
    ] {
        let original = event.clone();
        let source = crate::client::render::interactive_command_source(&event).unwrap();
        assert!(source.contains("last-source-marker") && source.contains("python3"));
        assert_eq!(event, original);
    }
}

#[test]
fn retained_command_sources_obey_per_event_and_shared_memory_limits() {
    let script = format!(
        "first-source-marker\n{}\nlast-source-marker",
        "界".repeat(MAX_DETAIL)
    );
    let mut view = EventView::new();
    for index in 0..20 {
        let event = json!({"type":"exec.started","job_id":format!("large-{index}"),
                          "command":["python3","-c",script]});
        let transcript = format!("[RUN] shell large-{index} python3 -c started\n");
        view.ingest_event("shell", &event, transcript.as_bytes());
        let retained = view.events.back().unwrap();
        assert_eq!(retained.text, transcript);
        assert!(
            !retained.truncated,
            "raw transcript has its own independent budget"
        );
        let source = retained.command_source.as_ref().unwrap();
        assert!(source.truncated);
        assert!(source.text.len() <= MAX_DETAIL);
        assert!(
            source.text.contains("first-source-marker")
                && source.text.contains("last-source-marker")
        );
        assert!(source.text.contains("/command JOB_ID"));
        assert_eq!(
            view.bytes,
            view.events.iter().map(Event::retained_bytes).sum::<usize>()
        );
        assert!(view.bytes <= MAX_BYTES);
    }
    assert!(
        view.dropped > 0,
        "display sources must count towards the shared retention budget"
    );
}
