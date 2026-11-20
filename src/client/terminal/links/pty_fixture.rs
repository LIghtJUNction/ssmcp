//! Explicit opt-in interactive fixture for scripts/e2e_links.py. It uses the
//! actual compositor, stdin parser, CallScope path and opener, with no relay,
//! model, user window or external URL request.
use super::*;
use crate::{
    backend::Backend,
    client::{interface, render, terminal, ui::Ui},
    output::OutputStore,
    runtime::{Runtime, call_scope::CallScope},
    session::attach::input::{Event, Keyboard},
    wire::{CodeContext, ControlAccount, ControlAttribution},
};
use serde_json::json;
use std::path::{Path, PathBuf};

fn seed(state: &mut terminal::State, mode: &str, scope: &Arc<CallScope>) {
    state.events.clear();
    state
        .events
        .set_scope(terminal::events::EventScope::Conversation);
    state.selection = false;
    state.help = false;
    state.conversation.view = terminal::conversation::View::Activity;
    state.notice = None;
    if mode == "emoji" {
        state.log_message("agent", "👨‍👩‍👧‍👦 [FAMILY_LINK](https://emoji.example.test/family)\n❤️ [HEART_LINK](https://emoji.example.test/heart)\n🇨🇳 [FLAG_LINK](https://emoji.example.test/flag)\n🧑🏽‍💻 [CODER_LINK](https://emoji.example.test/coder)", b"");
    } else if mode == "groups" {
        for n in 0..18 {
            state.log_message("user", &format!("earlier message {n}"), b"");
        }
        for n in 0..8 {
            let job = format!("fixture-{n}");
            state.log_scoped_event(
                "fixture",
                &json!({"type":"exec.started","job_id":job}),
                format!("[RUN] fixture #{job} fixture-command-{n}\n").as_bytes(),
                Some(scope.clone()),
            );
            let text = (0..60)
                .map(|line| format!("https://group.example.test/{n}/{line:03} 界\n"))
                .collect::<String>();
            state.log_scoped_event(
                "fixture",
                &json!({"type":"exec.stdout","job_id":job}),
                format!("[STDOUT] fixture #{job}\n{text}").as_bytes(),
                Some(scope.clone()),
            );
        }
    } else {
        let url = if mode == "replacement" {
            "https://osc.example.test/new"
        } else {
            "https://osc.example.test/old"
        };
        state.log_message("agent", &format!(
            "[LINK_LABEL界](https://markdown.example.test/a_(b))\nBare https://bare.example.test/path?q=1.\n\x1b]8;;{url}\x1b\\OSC_LABEL界{CLOSE}\nhttps://wrapped.example.test/long_path/with_many_parts/界?q=1&value=two"), b"");
        if mode == "anchored" {
            for n in 0..35 {
                state.log_message("user", &format!("filler line {n}"), b"");
            }
            let _ = state.chat_frame(Instant::now());
            state.events.scroll_by(-10000);
        }
    }
    state
        .conversation
        .composer
        .set_text("draft must survive 界");
    state.links.invalidate();
    state.dirty = true;
}

#[tokio::test]
#[ignore = "explicit private-PTY fixture; run scripts/e2e_links.py"]
async fn links_physical_pty_fixture() {
    let root = PathBuf::from(
        std::env::var_os("SSMCP_LINK_PTY_FIXTURE").expect("private fixture directory"),
    );
    assert!(root.is_absolute() && root.join("fixture.marker").is_file());
    let output = OutputStore::at_for_test(root.join("output")).unwrap();
    let backend = Backend::with_output(Path::new("/bin/sh"), root.clone(), output)
        .await
        .unwrap();
    let runtime = Runtime::new(backend, root.clone()).unwrap();
    let scope = runtime
        .call_scope(
            1,
            Some(&ControlAttribution {
                account: ControlAccount {
                    provider: "github".into(),
                    subject: "123".into(),
                    login: "private-fixture".into(),
                },
                owner: None,
                agent: None,
            }),
            Some(&CodeContext {
                run_id: "a".repeat(32),
                code: "native UI fixture".into(),
                timestamp_ms: 1,
                session_id: None,
                thread_id: None,
            }),
        )
        .unwrap();
    let ui = Ui {
        sidebar: false,
        ..Ui::default()
    };
    interface::initialize_path(Some(&root.join("ui.toml")));
    render::configure(ui.clone());
    let _display = terminal::Session::start(&ui, "private-link-fixture", None, None)
        .unwrap()
        .expect("owned foreground PTY");
    let mut keyboard = Keyboard::open(false).unwrap();
    {
        let shared = terminal::shared().unwrap();
        let mut state = shared.state.lock().unwrap();
        state.preview = true;
        seed(&mut state, "links", &scope);
    }
    let mut tick = tokio::time::interval(Duration::from_millis(25));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut sequence = 0;
    while Instant::now() < deadline {
        tokio::select! {
            event = keyboard.next() => match event.unwrap() {
                Event::Bytes(bytes) | Event::PasteBytes(bytes) => { let _ = terminal::keyboard_bytes(&bytes); }
                Event::InputProgress(now) => terminal::paste_input_progress(now),
                Event::PasteExpired => terminal::discard_image_paste(),
                Event::Resize(rows, cols) => { terminal::physical_resize(rows, cols); }
                Event::Interrupt | Event::Detach | Event::End => return,
            },
            _ = tick.tick() => {
                terminal::take_keyboard();
                interface::apply_pending().unwrap();
                let shared = terminal::shared().unwrap();
                let mut state = shared.state.lock().unwrap();
                if let Ok(raw) = std::fs::read(root.join("control.json"))
                    && let Ok(command) = serde_json::from_slice::<serde_json::Value>(&raw)
                    && let Some(next) = command["sequence"].as_u64()
                    && next != sequence {
                    sequence = next;
                    match command["op"].as_str() {
                        Some("seed") => seed(&mut state, command["mode"].as_str().unwrap_or("links"), &scope),
                        Some("append") => { state.log_message("user", "new background message", b""); }
                        Some("redraw") => { state.notice = Some("unrelated status update".into()); state.dirty = true; }
                        Some("quit") => return,
                        _ => {},
                    }
                }
                let snapshot = json!({"sequence":sequence,"draft":state.conversation.composer.text(),"selection":state.selection,
                    "ready":state.links.ready,"events":state.events.snapshot(),"links":state.links.shown.iter().map(|hit| json!({
                        "row":hit.row,"start":hit.hit.start,"end":hit.hit.end,"target":hit.hit.target.as_ref(),"event":hit.event,
                    })).collect::<Vec<_>>()});
                std::fs::write(root.join("state.next"), serde_json::to_vec(&snapshot).unwrap()).unwrap();
                std::fs::rename(root.join("state.next"), root.join("state.json")).unwrap();
            }
        }
    }
    panic!("private PTY driver exceeded its bounded fixture lifetime");
}
