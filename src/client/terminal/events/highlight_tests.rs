use super::*;

fn colored_ui() -> Ui {
    Ui {
        color: true,
        ..Ui::default()
    }
}

#[test]
fn raw_json_keeps_escaped_html_and_unicode_in_string_tones() {
    let ui = colored_ui();
    let fragment = r#""70: <article><h3>\u9080\u8bf7</h3><p>\"workers\"</p>\n71: C:\\work\\ssmcp""#;
    let source =
        format!(r#"{{"file":"src/portal/guide/zh.html","conflicts":[{fragment}]}}"#) + "\n";
    let styled = semantic_text(&source, &ui, "");

    assert_eq!(unstyled(&styled), source);
    assert!(styled.contains(&paint(r#""file""#, "heading", &ui, false)));
    assert!(styled.contains(&paint(
        r#""src/portal/guide/zh.html""#,
        "success",
        &ui,
        false
    )));
    assert!(styled.contains(&paint(fragment, "success", &ui, false)));

    let plain = Ui { color: false, ..ui };
    assert_eq!(semantic_text(&source, &plain, ""), source);
}

#[test]
fn raw_json_arrays_are_not_whole_line_log_labels() {
    let ui = colored_ui();
    let source = "[\"\\u4f60\\u597d\",\"</code>\",true,42]\n";
    let styled = semantic_text(source, &ui, "");

    assert_eq!(unstyled(&styled), source);
    assert!(styled.contains(&paint(r#""\u4f60\u597d""#, "success", &ui, false)));
    assert!(styled.contains(&paint(r#""</code>""#, "success", &ui, false)));
    assert!(styled.contains(&paint("true", "accent", &ui, true)));
    assert!(styled.contains(&paint("42", "warning", &ui, false)));
}

#[test]
fn plain_paths_log_labels_and_existing_colors_keep_their_styles() {
    let ui = colored_ui();
    let source = "opened C:\\work\\ssmcp src/main.rs\n[WARN] wait for the current claim\n";
    let styled = semantic_text(source, &ui, "");

    assert_eq!(unstyled(&styled), source);
    assert!(styled.contains(&paint("C:\\work\\ssmcp ", "blue", &ui, false)));
    assert!(styled.contains(&paint("src/main.rs\n", "blue", &ui, false)));
    assert!(styled.contains(&paint(
        "[WARN] wait for the current claim\n",
        "warning",
        &ui,
        false
    )));
    let existing = "\x1b[35m{\"file\":\"src/lib.rs\"}\x1b[0m\n";
    assert_eq!(semantic_text(existing, &ui, ""), existing);
}

#[test]
fn json_detection_requires_complete_data_without_an_explicit_language() {
    let ui = colored_ui();
    let incomplete = r#"{"file":"src/lib.rs"} trailing-output"#;
    let styled = semantic_text(incomplete, &ui, "");
    assert_eq!(unstyled(&styled), incomplete);
    assert!(!styled.contains(&paint(r#""file""#, "heading", &ui, false)));
    assert_eq!(semantic_text("true", &ui, ""), "true");

    let command = r#"[ -f C:\work\ssmcp ]"#;
    assert_eq!(unstyled(&semantic_text(command, &ui, "")), command);
    let explicit_shell = r#"{"file":"src/lib.rs"}"#;
    let styled = semantic_text(explicit_shell, &ui, "sh");
    assert_eq!(unstyled(&styled), explicit_shell);
    assert!(styled.contains(&paint(r#""file""#, "success", &ui, false)));
    assert!(!styled.contains(&paint(r#""file""#, "heading", &ui, false)));
}

#[test]
#[ignore = "exports actual terminal frames to SSMCP_HIGHLIGHT_SNAPSHOT_DIR"]
fn raw_json_visual_fixture() {
    use crate::client::ui::Theme;

    let directory = std::path::PathBuf::from(
        std::env::var_os("SSMCP_HIGHLIGHT_SNAPSHOT_DIR").expect("snapshot output directory"),
    );
    std::fs::create_dir_all(&directory).unwrap();
    let source = include_str!("highlight_fixture.txt");
    std::fs::write(directory.join("input.txt"), source).unwrap();

    // This is the renderer's effective NO_COLOR mode; no process-global
    // environment is changed by a test that can share the binary with others.
    let plain = semantic_text(
        source,
        &Ui {
            color: false,
            ..Ui::default()
        },
        "",
    );
    assert_eq!(plain.as_bytes(), source.as_bytes());
    assert!(!plain.contains('\x1b'));
    std::fs::write(directory.join("no-color.txt"), &plain).unwrap();

    let mut frames = Vec::new();
    for theme in [Theme::Dark, Theme::Light] {
        for cols in [48, 120] {
            let rows = 36;
            let ui = Ui {
                color: true,
                theme,
                ..Ui::default()
            };
            assert_eq!(unstyled(&semantic_text(source, &ui, "")), source);
            let mut state = super::super::State::new(rows, cols, &ui);
            state.preview = true;
            state.notice = Some("Synthetic highlighting fixture".into());
            // Exercise the real tool-output path. Agent conversation prose
            // intentionally goes through Markdown, not semantic_text.
            state.events.ingest_event(
                "res",
                &json!({"type":"tool_result","call_id":"highlight-fixture"}),
                format!("[RES] Highlighting fixture\n{source}").as_bytes(),
            );
            state.events.open();
            let now = std::time::Instant::now();
            let _ = state.frame(now);
            let frame = state.frame(now);
            assert_eq!(
                frame,
                state.frame(now),
                "same input and time must yield the same frame"
            );
            let stem = format!("highlight-{}-{cols}", theme.name());
            std::fs::write(directory.join(format!("{stem}.ansi")), frame.diff(None)).unwrap();
            std::fs::write(
                directory.join(format!("{stem}.json")),
                serde_json::to_vec(&json!({"rows": rows, "cols": cols})).unwrap(),
            )
            .unwrap();
            frames.push(json!({
                "stem": stem,
                "rows": rows,
                "cols": cols,
                "theme": theme.name(),
                "tones": {
                    "heading": ui.color("heading"),
                    "success": ui.color("success"),
                    "blue": ui.color("blue"),
                    "accent": ui.color("accent"),
                    "warning": ui.color("warning")
                }
            }));
        }
    }
    std::fs::write(
        directory.join("fixture.json"),
        serde_json::to_vec_pretty(&json!({
            "schema": "ssmcp.highlight-fixture.v1",
            "synthetic_input": true,
            "renderer": "State::frame / EventView::render (tool_result) / semantic_text / Frame::diff",
            "no_color_mode": "Ui.color=false (the renderer mode selected by NO_COLOR)",
            "byte_preservation": true,
            "frames": frames
        }))
        .unwrap(),
    )
    .unwrap();
}
