use super::*;
use sha2::{Digest, Sha256};

fn fixture(cols: u16) -> State {
    let mut state = preview(32, cols);
    state.events.clear();
    state.log_message("user", "Inspect these adjacent calls.", b"");
    for index in 0..4 {
        let id = format!("group-{index}");
        state.log_event(
            "preview",
            &json!({"type":"exec.started","job_id":id,"command":["cargo",format!("check-{index}")]}),
            format!("[RUN] preview {id} cargo check-{index} started\n").as_bytes(),
        );
        let output = (0..20)
            .map(|n| format!("call {index} result {n:02}\n"))
            .collect::<String>();
        state.log_event(
            "preview",
            &json!({"type":"exec.stdout","job_id":id}),
            output.as_bytes(),
        );
    }
    let script = format!(
        "print('head-marker')\nAPI_TOKEN=private-frame-secret\n{}\nprint('tail-marker')",
        "界".repeat(700)
    );
    state.log_event(
        "preview",
        &json!({"type":"exec.started","job_id":"long-script","command":["python3","-c",script]}),
        b"[RUN] preview long-script python3 -c started\n",
    );
    state.conversation.composer.set_text("Keep this draft");
    state.conversation.composer.left();
    state.notice = None;
    state
}

fn save_preview(state: &mut State, name: &str) {
    let Some(out) = std::env::var_os("SSMCP_TOOL_GROUP_PREVIEW_DIR") else {
        return;
    };
    let out = std::path::PathBuf::from(out);
    std::fs::create_dir_all(&out).unwrap();
    let frame = state.chat_frame(Instant::now());
    let name = format!("{name}-{}", frame.geometry.cols);
    let filename = format!("{name}.ansi");
    let ansi = frame.diff(None);
    std::fs::write(out.join(&filename), &ansi).unwrap();
    static BINARY_SHA: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    let binary_sha = BINARY_SHA.get_or_init(|| {
        let executable = std::env::current_exe().unwrap();
        format!("{:x}", Sha256::digest(std::fs::read(executable).unwrap()))
    });
    let source_revision = std::env::var("SSMCP_SOURCE_REVISION").ok();
    let mut manifest: serde_json::Value = std::fs::read(out.join("manifest.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or(serde_json::Value::Null);
    if manifest["source_revision"] != json!(source_revision)
        || manifest["binary_sha256"] != json!(binary_sha)
        || !manifest["frames"].is_array()
    {
        manifest = json!({
            "kind":"production_frame_fixture",
            "source_revision":source_revision,
            "binary_sha256":binary_sha,
            "frames":[],
        });
    }
    let frames = manifest["frames"].as_array_mut().unwrap();
    frames.retain(|entry| entry["path"] != filename);
    frames.push(json!({
        "path":filename,
        "rows":frame.geometry.rows,
        "cols":frame.geometry.cols,
        "sha256":format!("{:x}", Sha256::digest(&ansi)),
    }));
    std::fs::write(
        out.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
}

#[test]
fn adjacent_tool_group_frame_scroll_expand_and_command_tail_preserve_draft_and_queue() {
    for cols in [120, 48] {
        let mut state = fixture(cols);
        let draft = state.conversation.snapshot()["draft"].clone();
        let queue = state.chat_data()["queue"].clone();
        let screen = display(&mut state).screen().contents();
        assert!(screen.contains("Tools · 5 calls"), "{screen}");
        assert!(!screen.contains("private-frame-secret"));
        save_preview(&mut state, "group-folded");
        let header_y = screen
            .lines()
            .position(|row| row.contains("Tools · 5 calls"))
            .unwrap() as u16;
        assert_eq!(state.events.snapshot()["tool_groups"][0]["expanded"], false);
        assert!(!screen.contains("call 0 result"), "{screen}");
        state.chat_key(Key::Mouse {
            button: 0,
            x: 8,
            y: header_y,
            release: false,
        });
        display(&mut state);
        save_preview(&mut state, "group-expanded");
        let before = state.events.snapshot();
        assert_eq!(before["tool_groups"][0]["scroll"], 0);
        state.chat_key(Key::Mouse {
            button: 65,
            x: 8,
            y: header_y,
            release: false,
        });
        display(&mut state);
        let scrolled = state.events.snapshot();
        assert!(
            scrolled["tool_groups"][0]["scroll"].as_u64().unwrap()
                > before["tool_groups"][0]["scroll"].as_u64().unwrap()
        );
        assert_eq!(scrolled["scroll"], before["scroll"]);
        assert_eq!(scrolled["events"][1]["panel"], before["events"][1]["panel"]);
        save_preview(&mut state, "group-scrolled");
        state.chat_key(Key::Home);
        display(&mut state);
        assert_eq!(state.events.snapshot()["tool_groups"][0]["scroll"], 0);
        for _ in 0..4 {
            state.chat_key(Key::AuditDown);
        }
        display(&mut state);
        let selected_previous = state.events.snapshot()["selected"].clone();
        state.chat_key(Key::AuditDown);
        let screen = display(&mut state).screen().contents();
        let selected = state.events.snapshot()["selected"].clone();
        assert_ne!(selected, selected_previous);
        assert_eq!(selected, state.events.snapshot()["events"][5]["id"]);
        let title_y = screen
            .lines()
            .position(|row| row.contains("python3 -c"))
            .unwrap() as u16;
        state.chat_key(Key::Mouse {
            button: 0,
            x: 8,
            y: title_y,
            release: false,
        });
        let head = display(&mut state).screen().contents();
        assert!(head.contains("head-marker"), "{head}");
        assert!(!head.contains("private-frame-secret"));
        assert_eq!(state.events.snapshot()["events"][5]["expanded"], true);
        assert_eq!(state.events.snapshot()["events"][5]["panel"]["scroll"], 0);
        save_preview(&mut state, "group-command-head");

        let outer_offset = state.events.snapshot()["tool_groups"][0]["scroll"].clone();
        let inner_y = head
            .lines()
            .position(|row| row.contains("head-marker"))
            .unwrap() as u16;
        state.chat_key(Key::Mouse {
            button: 65,
            x: 8,
            y: inner_y,
            release: false,
        });
        display(&mut state);
        assert_eq!(state.events.snapshot()["events"][5]["panel"]["scroll"], 3);
        assert_eq!(
            state.events.snapshot()["tool_groups"][0]["scroll"],
            outer_offset
        );
        state.chat_key(Key::End);
        let tail = display(&mut state).screen().contents();
        assert!(tail.contains("tail-marker"), "{tail}");
        assert!(!tail.contains("private-frame-secret"));
        assert_eq!(
            state.events.snapshot()["tool_groups"][0]["scroll"],
            outer_offset
        );
        save_preview(&mut state, "group-command-tail");

        state.chat_key(Key::Home);
        let head_again = display(&mut state).screen().contents();
        assert!(head_again.contains("head-marker"), "{head_again}");
        let header_y = head_again
            .lines()
            .position(|row| row.contains("Tools · 5 calls"))
            .unwrap() as u16;
        state.chat_key(Key::Mouse {
            button: 0,
            x: 8,
            y: header_y,
            release: false,
        });
        display(&mut state);
        assert_eq!(state.events.snapshot()["tool_groups"][0]["expanded"], false);
        // Enter on the selected summary reopens only the display group.
        input(&mut state, b"\r");
        display(&mut state);
        assert_eq!(state.events.snapshot()["tool_groups"][0]["expanded"], true);
        assert_eq!(state.events.snapshot()["events"][5]["expanded"], true);
        assert_eq!(state.conversation.snapshot()["draft"], draft);
        assert_eq!(state.chat_data()["queue"], queue);
        assert!(state.actions.is_empty() && state.conversation.submitting.is_none());
        assert!(
            tail.lines()
                .all(|line| unicode_width::UnicodeWidthStr::width(line) <= usize::from(cols))
        );
    }
}
