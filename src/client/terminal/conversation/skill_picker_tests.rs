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
        state.chat_frame(Instant::now());
        let old_row = state.conversation.reference_rows[0].0;
        let old_height = state.geometry.input_rows;
        state.chat_resize_input(true);
        assert!(state.geometry.input_rows > old_height);
        assert!(
            !state.chat_reference_mouse(2, old_row),
            "a reference row drawn before composer resize is stale"
        );
        assert_eq!(state.conversation.composer.text(), "$c");
        state.chat_resize_input(false);
        state.chat_frame(Instant::now());
        assert_empty_queue(&queue, &mut transport);
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
        let prepared = steering::prepare_publish(id, expanded.clone().into())
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
        state.chat_updates();
        assert!(state.conversation.submitting.is_none());
        fs::write("notes.txt", "Existing file preview.").unwrap();
        let draft = "请检查 @notes.txt，@\"新 文件.rs\"，按 $chosen 和 $missing 完成";
        state.keyboard(draft.as_bytes(), Instant::now(), false);
        assert_eq!(state.conversation.composer.text(), draft);
        state.keyboard(b"\r", Instant::now(), false);
        assert!(state.conversation.composer.text().is_empty());
        let Some(steering::Transport::Wire { id }) = transport.recv().await else {
            panic!("missing references still publish the complete request");
        };
        let Some(SteeringCommand::Publish { text, .. }) = steering::pending_command(id) else {
            panic!("the missing-reference request must be publishable");
        };
        assert_eq!(text, draft);
        let expanded = runtime.expand_references(&text).await.unwrap();
        assert_eq!(references::source(&expanded), draft);
        assert_eq!(references::warnings(&expanded).len(), 2);
        assert!(expanded.contains("Existing file preview."));
        let prepared = steering::prepare_publish(id, expanded.clone().into())
            .unwrap()
            .unwrap();
        assert!(steering::sent(id));
        let acknowledged = queue.command(ROOM, vec![], prepared).unwrap();
        steering::reply(id, Ok(acknowledged)).unwrap();
        state.chat_updates();
        assert!(state.conversation.submitting.is_none());
        assert!(
            state
                .notice
                .as_deref()
                .unwrap()
                .contains("Queued · reference notice:")
        );
        assert!(state.conversation.composer.text().is_empty());

        let delivered = queue.claim(
            ROOM,
            "missing-reference-run",
            None,
            crate::steering::MAX_DELIVERY_MESSAGES,
            crate::steering::MAX_DELIVERY_BYTES,
        );
        assert_eq!(delivered.len(), 1);
        assert_eq!(delivered[0]["text"], expanded);
        assert_eq!(delivered[0]["source"], "client_user");
        assert_eq!(
            references::source(delivered[0]["text"].as_str().unwrap()),
            draft
        );
        assert!(
            queue
                .claim(
                    ROOM,
                    "missing-reference-repeat",
                    None,
                    crate::steering::MAX_DELIVERY_MESSAGES,
                    crate::steering::MAX_DELIVERY_BYTES,
                )
                .is_empty()
        );
        assert!(transport.try_recv().is_err());

        steering::disconnect();
        runtime.shutdown().await;
    });
}

async fn wait_for_send_reference_menu(state: &mut State) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            state.chat_refresh_completions();
            if !state.conversation.composer.completion_loading() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("local reference suggestions finish");
    assert!(state.chat_menu_visible());
    assert_eq!(state.conversation.composer.suggestions().len(), 1);
}

async fn finish_send_reference_receipt(state: &mut State) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            state.chat_poll_force_send();
            if state.conversation.force_task.is_none() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the accepted listener receipt reaches the composer");
    assert!(state.conversation.composer.is_empty());
}

#[test]
fn send_reference_picker_preserves_drafts_and_delivers_only_after_explicit_send() {
    use crate::client::interface::{harness, listener as receiver};
    use crate::wire::{ControlAccount, ControlAttribution};

    const CHILD: &str = "SSMCP_TEST_SEND_REFERENCE_PICKER";
    if std::env::var_os(CHILD).is_none() {
        let fixture = Fixture::new();
        fs::write(
            fixture.0.join("资料 文件.txt"),
            "Actual local file preview.\n",
        )
        .unwrap();
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "client::terminal::conversation::skill_picker_tests::send_reference_picker_preserves_drafts_and_delivers_only_after_explicit_send", "--nocapture"])
            .env(CHILD, "1")
            .env("XDG_STATE_HOME", fixture.0.join("state"))
            .env("XDG_CONFIG_HOME", fixture.0.join("config"))
            .current_dir(&fixture.0)
            .output()
            .unwrap();
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
            crate::output::OutputStore::at_for_test(root.join("output")).unwrap(),
        )
        .await
        .unwrap();
        let runtime = crate::runtime::Runtime::new(backend, root).unwrap();
        crate::client::completion::bind(&runtime);
        harness::bind(&runtime);
        receiver::connect("send-reference-fixture");
        // This in-process receiver has no relay, external recipient or model.
        let scope = runtime
            .call_scope(
                1,
                Some(&ControlAttribution {
                    account: ControlAccount {
                        provider: "github".into(),
                        subject: "424242".into(),
                        login: "fixture".into(),
                    },
                    owner: None,
                    agent: None,
                }),
                None,
            )
            .unwrap();
        let opened = scope
            .scope(receiver::call(json!({
                "op":"open", "label":"reference fixture",
                "idempotency_key":"send_reference_fixture"
            })))
            .await
            .unwrap();
        let id = opened["listener_id"].as_str().unwrap();
        let token = opened["token"].as_str().unwrap();
        let read = |wait_ms| {
            receiver::call(json!({
                "op":"read", "listener_id":id, "token":token, "wait_ms":wait_ms
            }))
        };
        let mut state = State::new(
            28,
            96,
            &Ui {
                layout: Layout::Chat,
                ..Ui::default()
            },
        );
        assert!(!state.preview);

        for (draft, selected, accept_key, kind) in [
            (
                "/send @资料",
                "/send @\"资料 文件.txt\" ",
                b"\r".as_slice(),
                "text",
            ),
            ("/send $ch", "/send $chosen ", b"\t".as_slice(), "skill"),
        ] {
            state.keyboard(draft.as_bytes(), Instant::now(), false);
            assert_eq!(state.conversation.composer.text(), draft);
            // Files and skills have separate uncached keys. Without yielding,
            // neither worker can finish before the first Enter.
            assert!(state.conversation.composer.completion_loading());
            state.keyboard(b"\r", Instant::now(), false);
            assert_eq!(state.conversation.composer.text(), draft);
            assert!(state.conversation.force_task.is_none());
            assert!(
                state
                    .notice
                    .as_deref()
                    .unwrap()
                    .contains("Loading reference suggestions")
            );
            assert!(receiver::snapshot()["pending"].is_null());
            assert!(scope.scope(read(0)).await.unwrap()["delivery"].is_null());

            wait_for_send_reference_menu(&mut state).await;
            state.keyboard(accept_key, Instant::now(), false);
            assert_eq!(state.conversation.composer.text(), selected);
            assert!(!state.chat_menu_visible());
            assert!(state.conversation.force_task.is_none());
            assert!(scope.scope(read(0)).await.unwrap()["delivery"].is_null());

            state.keyboard(b"\r", Instant::now(), false);
            assert!(state.conversation.force_task.is_some());
            assert_eq!(
                state.conversation.composer.text(),
                selected,
                "the selected reference stays visible until receiver ACK"
            );
            let delivery = scope.scope(read(5000)).await.unwrap()["delivery"].clone();
            let source = selected.strip_prefix("/send ").unwrap();
            assert_eq!(delivery["source_text"], source);
            assert_eq!(delivery["source"], "client_user");
            let expanded = delivery["text"].as_str().unwrap();
            assert_eq!(references::source(expanded), source);
            let encoded = expanded
                .split_once("<ssmcp-references-v1>\n")
                .unwrap()
                .1
                .strip_suffix("\n</ssmcp-references-v1>")
                .unwrap();
            let payload: Value = serde_json::from_str(encoded).unwrap();
            let attached = payload["references"].as_array().unwrap();
            assert_eq!(attached.len(), 1);
            assert_eq!(attached[0]["kind"], kind);
            if kind == "text" {
                assert_eq!(attached[0]["path"], "资料 文件.txt");
                assert_eq!(attached[0]["preview"], "Actual local file preview.\n");
            } else {
                assert_eq!(attached[0]["name"], "chosen");
                assert_eq!(attached[0]["path"], ".agents/skills/chosen/SKILL.md");
                assert!(!expanded.contains("unselected"));
                assert!(!expanded.contains("Private instructions"));
            }
            let ack = scope
                .scope(receiver::call(json!({
                    "op":"ack", "listener_id":id, "token":token,
                    "delivery_id":delivery["delivery_id"], "status":"accepted"
                })))
                .await
                .unwrap();
            assert_eq!(ack["receipt"]["status"], "accepted");
            finish_send_reference_receipt(&mut state).await;
            assert!(scope.scope(read(0)).await.unwrap()["delivery"].is_null());
        }

        let missing = "/send 请创建 @future-missing.txt ";
        state.keyboard(missing.as_bytes(), Instant::now(), false);
        state.keyboard(b"\r", Instant::now(), false);
        assert!(state.conversation.force_task.is_some());
        let delivery = scope.scope(read(5000)).await.unwrap()["delivery"].clone();
        let expanded = delivery["text"].as_str().unwrap();
        assert_eq!(
            references::source(expanded),
            missing.strip_prefix("/send ").unwrap()
        );
        assert_eq!(references::warnings(expanded).len(), 1);
        assert!(!Path::new("future-missing.txt").exists());
        scope
            .scope(receiver::call(json!({
                "op":"ack", "listener_id":id, "token":token,
                "delivery_id":delivery["delivery_id"], "status":"accepted"
            })))
            .await
            .unwrap();
        finish_send_reference_receipt(&mut state).await;
        assert!(
            state
                .notice
                .as_deref()
                .unwrap()
                .contains("future-missing.txt")
        );
        assert_eq!(
            receiver::snapshot()["receipts"].as_array().unwrap().len(),
            3
        );

        for group in [false, true] {
            state.conversation.native_recipient =
                (!group).then(|| "client:synthetic-native-recipient".into());
            state.conversation.send_to_group = group;
            for draft in ["/send @资料", "/send $ch"] {
                state.conversation.composer.set_text(draft);
                state.chat_refresh_completions();
                assert!(!state.conversation.composer.reference_active());
                assert!(!state.conversation.composer.completion_loading());
                assert!(state.conversation.composer.suggestions().is_empty());
                assert!(!state.chat_menu_visible());
                assert_eq!(state.conversation.composer.text(), draft);
                assert!(state.conversation.force_task.is_none());
            }
        }
        // Selecting a group only changes /send. Ordinary Enter still targets
        // the local request queue, so its skill picker remains available.
        assert!(state.conversation.send_to_group);
        assert!(state.conversation.native_recipient.is_none());
        state.conversation.composer.set_text("$ch");
        wait_for_send_reference_menu(&mut state).await;
        assert!(state.conversation.composer.reference_active());
        assert_eq!(
            state
                .conversation
                .composer
                .selected_suggestion()
                .unwrap()
                .name,
            "$chosen"
        );
        assert_eq!(state.conversation.composer.text(), "$ch");
        assert!(state.conversation.force_task.is_none());
        assert!(scope.scope(read(0)).await.unwrap()["delivery"].is_null());
        receiver::disconnect();
        runtime.shutdown().await;
    });
}
