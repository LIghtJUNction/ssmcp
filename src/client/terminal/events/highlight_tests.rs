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
