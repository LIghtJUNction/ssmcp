//! Exercise the real keyboard, local completion worker and queue delivery together.
use super::*;
use crate::wire::{SteeringCommand, SteeringState};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "ssmcp-skill-picker-input-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        for name in ["chosen", "unselected"] {
            let path = root.join(".agents/skills").join(name);
            fs::create_dir_all(&path).unwrap();
            fs::write(path.join("SKILL.md"), format!(
                "---\nname: {name}\ndescription: fixture {name}\n---\nPrivate instructions for {name}.\n"
            )).unwrap();
        }
        Self(root)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

const ROOM: &str = "skill-picker-fixture";

fn assert_empty_queue(
    queue: &crate::steering::Queue,
    transport: &mut tokio::sync::mpsc::Receiver<steering::Transport>,
) {
    assert!(
        queue
            .command(ROOM, vec![], SteeringCommand::Snapshot {})
            .unwrap()
            .messages
            .is_empty()
    );
    assert!(steering::snapshot()["queue"].as_array().unwrap().is_empty());
    assert!(
        matches!(
            transport.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ),
        "discovery and selection must not publish a request"
    );
}

#[test]
fn skill_picker_keys_only_deliver_the_explicitly_sent_selection() {
    const CHILD: &str = "SSMCP_TEST_SKILL_PICKER_INPUT";
    if std::env::var_os(CHILD).is_none() {
        let fixture = Fixture::new();
        // Completion, queue and history globals are isolated from parallel unit
        // tests, and any local history writes stay in this owned fixture.
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "client::terminal::conversation::skill_picker_tests::skill_picker_keys_only_deliver_the_explicitly_sent_selection", "--nocapture"])
            .env(CHILD, "1")
            .env("XDG_STATE_HOME", fixture.0.join("state"))
            .current_dir(&fixture.0)
            .output().unwrap();
        assert!(
            output.status.success(),
            "stdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    executor.block_on(async {
        let root = std::env::current_dir().unwrap();
        let backend = crate::backend::Backend::with_output(
            Path::new("/bin/sh"),
            root.clone(),
            crate::output::OutputStore::new_connection().unwrap(),
        )
        .await
        .unwrap();
        let runtime = crate::runtime::Runtime::new(backend, root).unwrap();
        crate::client::completion::bind(&runtime);
        let queue = crate::steering::Queue::default();
        let mut transport = steering::connect(ROOM, crate::wire::VERSION);
        let Some(steering::Transport::Wire { id }) = transport.recv().await else {
            panic!("a new connection requests its real queue snapshot");
        };
        let command = steering::pending_command(id).unwrap();
        assert!(matches!(command, SteeringCommand::Snapshot {}));
        assert!(steering::sent(id));
        steering::reply(id, queue.command(ROOM, vec![], command)).unwrap();
        let mut state = State::new(
            28,
            96,
            &Ui {
                layout: Layout::Chat,
                ..Ui::default()
            },
        );
        assert!(!state.preview);
        assert_empty_queue(&queue, &mut transport);

        // No yield occurs between typing and Enter, so the asynchronous worker
        // cannot have finished. Both input paths must preserve the same draft.
        state.keyboard(b"$", Instant::now(), false);
        assert_eq!(state.conversation.composer.text(), "$");
        assert!(state.conversation.composer.completion_loading());
        state.keyboard(b"\r", Instant::now(), false);
        assert_eq!(state.conversation.composer.text(), "$");
        assert!(
            state
                .notice
                .as_deref()
                .unwrap()
                .contains("Loading reference suggestions")
        );
        assert_empty_queue(&queue, &mut transport);
        let now = Instant::now();
        state.keyboard(b"\x1b", now, false);
        state.keyboard(&[], now + Duration::from_millis(100), true);
        assert_eq!(state.conversation.composer.text(), "$");
        assert!(!state.chat_menu_visible());
        assert_empty_queue(&queue, &mut transport);

        // Let the real in-flight worker finish after Esc. Its stale result may
        // populate the cache, but may neither reopen the menu nor publish.
        let query = crate::client::completion::query("$", 1).unwrap();
        let completed = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let completed = crate::client::completion::lookup(&query);
                if !completed.loading {
                    break completed;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("local fixture completions finish");
        assert_eq!(completed.items.len(), 2);
        state.chat_refresh_completions();
        assert_eq!(state.conversation.composer.text(), "$");
        assert!(!state.chat_menu_visible());
        assert_empty_queue(&queue, &mut transport);

        state.keyboard(b"c", Instant::now(), false);
        assert!(state.chat_menu_visible());
        assert_eq!(state.conversation.composer.suggestions().len(), 1);
        state.keyboard(b"\r", Instant::now(), false);
        assert_eq!(state.conversation.composer.text(), "$chosen ");
        assert!(!state.chat_menu_visible());
        assert_empty_queue(&queue, &mut transport);

        state.keyboard(b"\r", Instant::now(), false);
        assert!(state.conversation.composer.text().is_empty());
        let Some(steering::Transport::Wire { id }) = transport.recv().await else {
            panic!("only the explicit second Enter publishes");
        };
        let Some(SteeringCommand::Publish { text, .. }) = steering::pending_command(id) else {
            panic!("the queued operation is a publish");
        };
        assert_eq!(text.trim(), "$chosen");
        let expanded = runtime.expand_references(&text).await.unwrap();
        assert_eq!(references::source(&expanded), text);
        let encoded = expanded
            .split_once("<ssmcp-references-v1>\n")
            .unwrap()
            .1
            .strip_suffix("\n</ssmcp-references-v1>")
            .unwrap();
        let payload: Value = serde_json::from_str(encoded).unwrap();
        let references = payload["references"].as_array().unwrap();
        assert_eq!(references.len(), 1);
        assert_eq!(references[0]["kind"], "skill");
        assert_eq!(references[0]["name"], "chosen");
        assert_eq!(references[0]["path"], ".agents/skills/chosen/SKILL.md");
        assert!(!expanded.contains("unselected"));
        assert!(!expanded.contains("Private instructions"));
        let prepared = steering::prepare_publish(id, expanded.clone())
            .unwrap()
            .unwrap();
        assert!(steering::sent(id));
        let acknowledged = queue.command(ROOM, vec![], prepared).unwrap();
        assert_eq!(acknowledged.messages.len(), 1);
        assert_eq!(acknowledged.messages[0].state, SteeringState::Queued);
        steering::reply(id, Ok(acknowledged)).unwrap();

        let delivered = queue.claim(
            ROOM,
            "picker-run",
            None,
            crate::steering::MAX_DELIVERY_MESSAGES,
            crate::steering::MAX_DELIVERY_BYTES,
        );
        assert_eq!(delivered.len(), 1);
        assert_eq!(delivered[0]["text"], expanded);
        assert_eq!(delivered[0]["source"], "client_user");
        let completed = queue
            .command(ROOM, vec![], SteeringCommand::Snapshot {})
            .unwrap();
        assert_eq!(completed.messages.len(), 1);
        assert_eq!(completed.messages[0].state, SteeringState::Delivered);
        assert!(
            queue
                .claim(
                    ROOM,
                    "picker-run-2",
                    None,
                    crate::steering::MAX_DELIVERY_MESSAGES,
                    crate::steering::MAX_DELIVERY_BYTES
                )
                .is_empty()
        );
        assert!(
            transport.try_recv().is_err(),
            "the picker did not create a second request"
        );
        steering::disconnect();
        runtime.shutdown().await;
    });
}
