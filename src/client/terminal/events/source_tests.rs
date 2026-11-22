use super::*;
use crate::{
    backend::Backend,
    output::OutputStore,
    registry::Provider,
    runtime::{Runtime, call_scope::CallScope},
    wire::{CodeContext, ControlAccount, ControlAttribution, Operation},
};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(1);
// These tests share the process-wide copy cache even while using separate in-memory session fixtures.
static HISTORY_FIXTURES: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn runtime() -> (Scratch, Arc<Backend>, Arc<Runtime>) {
    let root = Scratch(std::env::temp_dir().canonicalize().unwrap().join(format!(
        "ssmcp-event-source-{}-{}",
        std::process::id(),
        NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
    )));
    std::fs::create_dir(&root.0).unwrap();
    let output = OutputStore::at_for_test(root.0.join("output")).unwrap();
    let backend = Backend::with_output(Path::new("/bin/sh"), root.0.clone(), output)
        .await
        .unwrap();
    let runtime = Runtime::new(backend.clone(), root.0.clone()).unwrap();
    (root, backend, runtime)
}

fn scope(runtime: &Runtime, request_id: u64, subject: &str, run: char) -> Arc<CallScope> {
    runtime
        .call_scope(
            request_id,
            Some(&ControlAttribution {
                account: ControlAccount {
                    provider: "github".into(),
                    subject: subject.into(),
                    login: "same-label".into(),
                },
                owner: None,
                agent: None,
            }),
            Some(&CodeContext {
                run_id: run.to_string().repeat(32),
                code: "native fixture".into(),
                timestamp_ms: 1,
                session_id: None,
                thread_id: None,
            }),
        )
        .unwrap()
}

fn start(fixture: &str) -> Operation {
    Operation::CallTool {
        name: "ssmcp.exec.start".into(),
        arguments: json!({
            "command": [std::env::current_exe().unwrap(), "--exact",
                "backend::shell::tests::fixture_process", "--nocapture"],
            "env": {"SSMCP_SHELL_TEST_FIXTURE": fixture},
            "timeout_ms": 3000,
        }),
    }
}

fn call(name: &str, id: &str) -> Operation {
    Operation::CallTool {
        name: name.into(),
        arguments: json!({"job_id":id}),
    }
}

fn job(value: &Value) -> String {
    value["structuredContent"]["job_id"]
        .as_str()
        .unwrap()
        .into()
}

#[tokio::test]
async fn call_scope_native_exec_keeps_creator_through_concurrency_reads_and_exit_projection() {
    let (_root, backend, runtime) = runtime().await;
    let projections = backend.observe_exec_projection();
    let alice = scope(&runtime, 1, "123", 'a');
    let bob = scope(&runtime, 2, "456", 'b');
    let (a, b) = tokio::join!(
        alice.scope(runtime.perform(start("alpha"))),
        bob.scope(runtime.perform(start("beta"))),
    );
    let a = job(&a.unwrap());
    let b = job(&b.unwrap());
    // A new task receives no ambient source, even while Bob is active.
    let local_runtime = runtime.clone();
    let local = bob
        .scope(async {
            tokio::spawn(async move { local_runtime.perform(start("retained")).await })
                .await
                .unwrap()
                .unwrap()
        })
        .await;
    let local = job(&local);

    for name in ["ssmcp.exec.status", "ssmcp.exec.read"] {
        bob.scope(runtime.perform(call(name, &a))).await.unwrap();
    }
    for id in [&a, &b, &local] {
        runtime
            .perform(Operation::CallTool {
                name: "ssmcp.exec.wait".into(),
                arguments: json!({"job_id":id,"wait_ms":5000}),
            })
            .await
            .unwrap();
    }

    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let mut observed: Vec<crate::backend::events::ProjectedEvent> = Vec::new();
    while observed
        .iter()
        .filter(|(event, _)| event["type"] == "exec.exited")
        .count()
        < 3
    {
        observed.extend(projections.try_iter());
        assert!(
            tokio::time::Instant::now() < deadline,
            "missing native exit projection"
        );
        tokio::time::sleep(Duration::from_millis(1)).await;
    }

    let mut view = EventView::default();
    for (event, source) in observed {
        let id = event["job_id"].as_str().unwrap();
        let expected = if id == a {
            Some(&alice)
        } else if id == b {
            Some(&bob)
        } else {
            assert_eq!(id, local);
            None
        };
        assert_eq!(source.call.is_some(), expected.is_some());
        if let Some(expected) = expected {
            assert!(source.call.as_ref().unwrap().same_call(expected));
        }
        // Projection metadata is not injected into the original audit payload.
        assert!(event.get("source").is_none());
        assert!(event.get("attribution").is_none());
        let text = format!("[RUN] {id} {}\n", event["type"].as_str().unwrap());
        view.ingest_scoped_event("shell", &event, text.as_bytes(), source.display());
    }
    assert_eq!(view.events.len(), 3);
    for id in [&a, &b, &local] {
        let event = view
            .events
            .iter()
            .find(|event| event.meta.keys.iter().any(|key| key.ends_with(id.as_str())))
            .unwrap();
        assert_eq!(event.meta.status, Some(Status::Exit(0)));
        let expected = if id == &a {
            Some(&alice)
        } else if id == &b {
            Some(&bob)
        } else {
            None
        };
        assert_eq!(event.meta.scope.is_some(), expected.is_some());
        if let Some(expected) = expected {
            assert!(event.meta.scope.as_ref().unwrap().same_call(expected));
        }
    }
    runtime.shutdown().await;
}

#[tokio::test]
async fn call_scope_provider_json_and_invalid_routes_cannot_claim_a_job_creator() {
    let (_root, backend, runtime) = runtime().await;
    let alice = scope(&runtime, 1, "123", 'a');
    let mut forged_route = backend.registry.lookup("ssmcp.exec.start").unwrap();
    forged_route.provider = Provider::Workspace;
    assert!(
        alice
            .scope(backend.dispatch(
                &forged_route,
                json!({"command":["this must never execute"]}),
            ))
            .await
            .is_err()
    );

    let mut view = EventView::default();
    let native = json!({"type":"exec.started","job_id":"owned","command":["native"]});
    let source = alice
        .scope(async { crate::runtime::call_scope::current_source() })
        .await;
    view.ingest_scoped_event("shell", &native, b"[RUN] native\n", source.display());
    let forged = json!({
        "type":"exec.exited","job_id":"owned","exit_code":99,
        "source":{"connection":alice.connection(),"request_id":alice.request_id()},
        "attribution":alice.attribution(),
        "run_id":alice.run_id(),"controller":{"name":"same-label"},
    });
    view.ingest_event("shell", &forged, b"[ERR] provider claim\n");
    assert_eq!(view.events.len(), 2);
    assert!(
        view.events[0]
            .meta
            .scope
            .as_ref()
            .unwrap()
            .same_call(&alice)
    );
    assert_eq!(view.events[0].meta.status, Some(Status::Running));
    assert!(view.events[1].meta.scope.is_none());
    assert_eq!(view.events[1].meta.status, Some(Status::Exit(99)));
    runtime.shutdown().await;
}

#[tokio::test]
async fn call_scope_file_workers_keep_source_but_local_discovery_stays_out_of_chat() {
    let (root, _backend, runtime) = runtime().await;
    let alice = scope(&runtime, 1, "123", 'a');
    std::fs::write(root.0.join("note.txt"), "private fixture").unwrap();
    let output = OutputStore::at_for_test(root.0.join("file-output")).unwrap();
    let workspace = crate::workspace::Workspace::with_output(root.0.clone(), output).unwrap();
    let presentations = workspace.observe_projection();
    alice
        .scope(workspace.perform("ssmcp.fs.read", json!({"path":"note.txt","max_bytes":64})))
        .await
        .unwrap();
    let (event, retained) = presentations.try_recv().unwrap();
    assert!(retained.call.unwrap().same_call(&alice));
    assert!(event.get("attribution").is_none());
    assert!(presentations.try_recv().is_err());
    alice
        .scope(workspace.read_metadata_for_completion("note.txt", 64))
        .await
        .unwrap();
    assert!(presentations.try_recv().is_err());
    let audit = workspace
        .perform("ssmcp.fs.events", json!({}))
        .await
        .unwrap();
    assert!(audit.to_string().contains("note.txt"));
    assert!(crate::runtime::call_scope::current().is_none());
    workspace.shutdown().await;
    runtime.shutdown().await;
}

fn verified(runtime: &Runtime, request_id: u64, subject: &str, agent: char) -> Arc<CallScope> {
    verified_with_labels(
        runtime,
        request_id,
        subject,
        agent,
        ("same-label", "same display name"),
    )
}

fn verified_with_labels(
    runtime: &Runtime,
    request_id: u64,
    subject: &str,
    agent: char,
    (login, name): (&str, &str),
) -> Arc<CallScope> {
    let thread = format!("agent_{}", agent.to_string().repeat(32));
    runtime
        .call_scope(
            request_id,
            Some(&ControlAttribution {
                account: ControlAccount {
                    provider: "github".into(),
                    subject: subject.into(),
                    login: login.into(),
                },
                owner: None,
                agent: Some(crate::wire::ControlAgent {
                    id: thread.clone(),
                    name: name.into(),
                }),
            }),
            Some(&CodeContext {
                run_id: format!("{request_id:032x}"),
                code: "synthetic history routing fixture".into(),
                timestamp_ms: 1,
                session_id: Some("controlled_client_fixture".into()),
                thread_id: Some(thread),
            }),
        )
        .unwrap()
}

async fn native_call(runtime: &Runtime, name: &str, arguments: Value) -> Result<Value, String> {
    let value = runtime
        .perform(Operation::CallTool {
            name: name.into(),
            arguments,
        })
        .await?;
    assert_ne!(value["isError"], true, "{value}");
    Ok(value["structuredContent"].clone())
}

fn seed_two_histories() -> (String, String) {
    use crate::client::interface::history;
    history::record_request("fixture_b", "B fixture request").unwrap();
    let b = history::current().unwrap().unwrap().metadata.id;
    history::release_current().unwrap();
    history::record_request("fixture_a", "A fixture request").unwrap();
    let a = history::current().unwrap().unwrap().metadata.id;
    (a, b)
}

async fn replay_native_events(
    state: &mut super::super::State,
    receiver: &std::sync::mpsc::Receiver<crate::backend::events::ProjectedEvent>,
    until: impl Fn(&Value, &[u8]) -> bool,
) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            for (event, source) in receiver.try_iter() {
                let bytes =
                    crate::client::render::render_event_for_test("shell", &event, 100).unwrap();
                // Use the same State boundary as live output, including the raw Logs buffer.
                state.log_scoped_event("shell", &event, &bytes, source.display());
                if until(&event, &bytes) {
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("the fixture did not publish the expected native event");
}

#[tokio::test]
async fn native_job_late_output_stays_with_creator_history_and_in_global_logs_and_tasks() {
    use crate::client::interface::history;

    let _serial = HISTORY_FIXTURES.lock().await;
    let (root, backend, runtime) = runtime().await;
    history::with_test_history(root.0.join("history"), async {
        let (a, b) = seed_two_histories();
        let projected = backend.observe_exec_projection();
        let mut state = super::super::State::new(
            35,
            120,
            &Ui { color: false, ..Ui::default() },
        );
        state.events.set_scope(EventScope::Conversation);
        // No verified caller. The native request must capture the selected local owner.
        let job = native_call(
            &runtime,
            "ssmcp.exec.start",
            json!({
                "command": ["/bin/sh", "-c", "printf 'A_READY\\n'; IFS= read -r line || exit 1; printf '%s\\n' \"$line\"; printf 'ERR:%s\\n' \"$line\" >&2"],
                "stdin": "pipe", "timeout_ms": 15000
            }),
        )
        .await
        .unwrap();
        let id = job["job_id"].as_str().unwrap();
        replay_native_events(&mut state, &projected, |event, bytes| {
            event["type"] == "exec.stdout" && String::from_utf8_lossy(bytes).contains("A_READY")
        })
        .await;
        state.resume_history_for_test(&b);
        let write = native_call(
            &runtime, "ssmcp.exec.write",
            json!({"job_id":id,"data":"A_LATE\n","close_stdin":true}),
        )
        .await
        .unwrap();
        assert_eq!(write["written"], 7);
        replay_native_events(&mut state, &projected, |event, _| event["type"] == "exec.exited")
            .await;

        let b_rendered = state.events.render(100, 30, &state.ui, false).join("\n");
        assert!(b_rendered.contains("B fixture request"), "{b_rendered}");
        assert!(!b_rendered.contains("A_LATE"), "{b_rendered}");
        assert!(!b_rendered.contains("A_READY"), "{b_rendered}");
        let job_cards = state.events.events.iter()
            .filter(|event| event.meta.keys.iter().any(|key| key.ends_with(id)))
            .collect::<Vec<_>>();
        assert_eq!(job_cards.len(), 1);
        assert!(job_cards[0].text.contains("A_LATE"));
        assert!(job_cards[0].text.contains("ERR:A_LATE"));
        let chat = state.events.visible_ids();
        assert!(!state.events.events.iter().any(|event| {
            chat.contains(&event.id) && event.text.contains("A_LATE")
        }));

        state.log_query = "a_ready".into();
        assert!(state.filtered_logs().unwrap().join("\n").contains("A_READY"));
        state.log_query = "a_late".into();
        assert!(state.filtered_logs().unwrap().join("\n").contains("A_LATE"));
        let tasks = native_call(&runtime, "ssmcp.exec.list", json!({})).await.unwrap();
        let task = tasks["jobs"].as_array().unwrap().iter()
            .find(|job| job["job_id"] == id).unwrap();
        assert_eq!(task["exit_code"], 0);
        let output = native_call(&runtime, "ssmcp.exec.read", json!({"job_id":id})).await.unwrap();
        assert!(output["stdout"].as_str().unwrap().contains("A_LATE"));
        assert!(output["stderr"].as_str().unwrap().contains("ERR:A_LATE"));
        let audit = native_call(&runtime, "ssmcp.exec.events", json!({})).await.unwrap();
        assert!(audit.to_string().contains("A_LATE"), "{audit}");

        // Repeated rehydration preserves the creator's native correlation and one saved card.
        for _ in 0..2 {
            for id in [&a, &b] {
                state.resume_history_for_test(id);
                assert_eq!(state.events.bytes, state.events.events.iter().map(Event::retained_bytes).sum::<usize>());
                let visible = state.events.visible_ids();
                let visible_events = state.events.events.iter()
                    .filter(|event| visible.contains(&event.id))
                    .collect::<Vec<_>>();
                assert_eq!(visible_events.iter().filter(|event| event.meta.history_message).count(), 1);
                assert_eq!(visible_events.iter().any(|event| event.text.contains("A_LATE")), id == &a);
            }
        }
    }).await;
    runtime.shutdown().await;
}

#[tokio::test]
async fn native_images_keep_distinct_cards_and_call_ownership_after_summary_correlation() {
    use crate::client::interface::history;
    use crate::runtime::call_scope::Source;
    use base64::{Engine as _, engine::general_purpose::STANDARD};

    let _serial = HISTORY_FIXTURES.lock().await;
    let (root, _backend, runtime) = runtime().await;
    history::with_test_history(root.0.join("image-history"), async {
        history::record_request("image_fixture_a", "A image request").unwrap();
        let a_call = verified(&runtime, 71, "123", 'a');
        let a = a_call.scope(async { Source::capture() }).await;
        history::release_current().unwrap();
        history::record_request("image_fixture_b", "B keeps reading").unwrap();
        let b_call = verified(&runtime, 72, "456", 'b');
        let b = b_call.scope(async { Source::capture() }).await;
        let a_history = a.require_owner().unwrap().display_id();
        let b_history = b.require_owner().unwrap().display_id();
        assert_ne!(a_history, b_history);

        let mut view = EventView::new();
        view.set_scope(EventScope::Conversation);
        view.set_history_owner(Some(b_history.clone()));
        view.summary_call_started(&a, "ssmcp.fs.read_image");
        let summary_id = view.events.back().unwrap().id;
        view.ingest_scoped_event(
            "message",
            &json!({"type":"ssmcp.message","text":"B keeps reading this reply"}),
            b"",
            b.display(),
        );
        let b_id = view.events.back().unwrap().id;
        assert_ne!(
            summary_id, b_id,
            "the correlated call is not the back event"
        );
        view.follow = false;
        view.selected = Some(b_id);
        let old_scroll = view.scroll;

        let block = json!({"type":"image","mimeType":"image/png",
            "data":STANDARD.encode(crate::client::input::paste::fixture_png())});
        let preview = || images::decode(&block, true).unwrap();
        view.ingest_image(b"", preview(), Some(a.display()));
        assert_eq!(view.events.len(), 2, "empty image placeholders are skipped");
        assert!(view.events.iter().all(|event| event.image.is_none()));

        for label in ["first", "second"] {
            view.ingest_image(
                format!("[IMAGE] {label}\n").as_bytes(),
                preview(),
                Some(a.display()),
            );
        }
        view.summary_call_finished(&a, true);

        let images = view
            .events
            .iter()
            .filter(|event| event.image.is_some())
            .collect::<Vec<_>>();
        assert_eq!(
            images.len(),
            2,
            "two images from one call retain two previews"
        );
        assert_ne!(images[0].id, images[1].id);
        for (event, label) in images.iter().zip(["first", "second"]) {
            assert!(event.text.contains(label));
            assert_eq!(event.meta.history, DisplayHistory::Local(a_history.clone()));
            assert!(event.meta.scope.as_ref().unwrap().same_call(&a_call));
            assert_eq!(event.meta.kind, Kind::Tool);
            assert!(event.image.as_ref().unwrap().native.is_some());
            assert!(event.tool_group_id.is_none());
        }
        let image_ids = images.iter().map(|event| event.id).collect::<Vec<_>>();
        let summary = view
            .events
            .iter()
            .find(|event| event.id == summary_id)
            .unwrap();
        assert!(
            summary.image.is_none(),
            "image payloads never replace the logical call row"
        );
        assert!(!summary.text.contains("[IMAGE]"));
        assert_eq!(summary.meta.status, Some(Status::Done));
        assert_eq!(
            view.tool_summary_list(&a).unwrap()["groups"][0]["call_count"],
            1
        );
        assert_eq!(view.visible_ids(), vec![b_id]);
        assert_eq!(view.selected, Some(b_id));
        assert_eq!(view.scroll, old_scroll);
        assert!(!view.follow);
        assert!(view.new_image.is_none());

        view.set_history_owner(Some(a_history));
        let visible = view.visible_ids();
        assert!(visible.contains(&summary_id));
        assert!(image_ids.iter().all(|id| visible.contains(id)));
        assert!(!visible.contains(&b_id));
        view.set_history_owner(Some(b_history));
        assert_eq!(view.visible_ids(), vec![b_id]);
    })
    .await;
    runtime.shutdown().await;
}

#[test]
fn late_native_image_uses_its_own_placeholder_and_owner() {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    let a = HistoryId::parse("conversation_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap();
    let b = HistoryId::parse("conversation_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb").unwrap();
    let mut state = super::super::State::new(30, 100, &Ui::default());
    state.events.set_scope(EventScope::Conversation);
    state.events.set_history_owner(Some(b.clone()));
    state.events.ingest_event(
        "message",
        &json!({"type":"ssmcp.message","text":"B's newest reply"}),
        b"",
    );
    let b_card = state.events.visible_ids()[0];

    let block = json!({"type":"image","mimeType":"image/png",
        "data":STANDARD.encode(crate::client::input::paste::fixture_png())});
    let preview = images::decode(&block, false).unwrap();
    let source = DisplaySource {
        history: DisplayHistory::Local(a.clone()),
        call: None,
    };
    state
        .events
        .ingest_image(b"", preview.clone(), Some(source.clone()));
    assert_eq!(state.events.events.len(), 1, "no placeholder was inserted");
    assert!(state.events.events[0].image.is_none());
    state.image(preview, "image/png", Some(source));

    assert_eq!(state.events.visible_ids(), vec![b_card]);
    assert!(state.events.events[0].image.is_none());
    state.events.set_history_owner(Some(a));
    let a_card = state.events.events.back().unwrap();
    assert_eq!(state.events.visible_ids(), vec![a_card.id]);
    assert!(a_card.image.is_some());
    assert!(state.logs.screen().contents().contains("Image attachment"));
}

#[test]
fn legacy_attach_shared_cards_stay_out_of_explicitly_selected_local_history() {
    let mut state = super::super::State::new(30, 100, &Ui::default());
    state.events.set_scope(EventScope::Conversation);
    state.follow_attach_history(DisplayHistory::LegacyAttach);
    let legacy = DisplaySource {
        history: DisplayHistory::LegacyAttach,
        call: None,
    };
    state.log_scoped_event(
        "agent",
        &json!({"type":"ssmcp.message","text":"Legacy shared reply"}),
        b"Legacy shared reply\n",
        legacy.clone(),
    );
    state.log_scoped_event(
        "shell",
        &json!({"type":"exec.started","job_id":"unowned-log"}),
        b"[RUN] Unowned native job\n",
        DisplaySource::default(),
    );
    let rows = state.events.render(100, 20, &state.ui, false).join("\n");
    assert!(rows.contains("Legacy shared reply"), "{rows}");
    assert!(!rows.contains("Unowned native job"), "{rows}");
    assert!(state.events.events[0].meta.history.local_id().is_none());

    let b = HistoryId::parse("conversation_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb").unwrap();
    state.events.set_history_owner(Some(b.clone()));
    state.events.ingest_event(
        "message",
        &json!({"type":"ssmcp.message","text":"Local B reply"}),
        b"",
    );
    // Another page from the same legacy backend must preserve the local selection.
    state.follow_attach_history(DisplayHistory::LegacyAttach);
    state.log_scoped_event(
        "agent",
        &json!({"type":"ssmcp.message","text":"Late legacy reply"}),
        b"Late legacy reply\n",
        legacy,
    );
    let rows = state.events.render(100, 20, &state.ui, false).join("\n");
    assert!(rows.contains("Local B reply"), "{rows}");
    assert!(!rows.to_lowercase().contains("legacy"), "{rows}");
    assert!(state.logs.screen().contents().contains("Late legacy reply"));

    state.follow_attach_history(DisplayHistory::NoSelection);
    assert!(state.events.is_empty());
    state.events.set_scope(EventScope::All);
    assert_eq!(state.events.visible_ids().len(), 4);

    let mut waiting = super::super::State::new(30, 100, &Ui::default());
    waiting.follow_attach_history(DisplayHistory::NoSelection);
    waiting.events.set_history_owner(Some(b.clone()));
    waiting.follow_attach_history(DisplayHistory::LegacyAttach);
    assert_eq!(waiting.events.history_selection, DisplayHistory::Local(b));
}

async fn verified_reply_routing(name: &str, arguments: Value) {
    use crate::{
        client::interface::{history, messages},
        session::attach,
    };

    let _serial = HISTORY_FIXTURES.lock().await;
    let (root, _, runtime) = runtime().await;
    let path = root.0.join("owner-history");
    history::with_test_history(path.clone(), async {
        let (a, b) = seed_two_histories();
        let capture = attach::Handle::capture_for_test();
        let previous = verified(&runtime, 1, "123", 'a');
        // Bind the verified thread at its observation on A.
        assert_eq!(
            previous
                .scope(async {
                    crate::runtime::call_scope::current_source()
                        .require_owner()
                        .unwrap()
                        .id()
                        .to_owned()
                })
                .await,
            a,
        );
        history::resume_test_history(&b).unwrap();
        let second = verified(&runtime, 2, "456", 'b');
        let b_id = native_call(
            &runtime,
            "ssmcp.ui.message",
            json!({"text":"B current reply"}),
        )
        .await
        .unwrap()["message_id"]
            .as_str()
            .unwrap()
            .to_owned();
        let b_messages = history::read(&b).unwrap().messages;
        assert!(!path.exists());

        // A fresh relay call after the switch uses the original authenticated binding.
        let late =
            verified_with_labels(&runtime, 3, "123", 'a', ("renamed-login", "renamed agent"));
        assert!(!late.same_call(&previous));
        let first = late
            .scope(native_call(&runtime, name, arguments.clone()))
            .await
            .unwrap();
        assert_eq!(first["recorded"], true);
        assert_eq!(first["storage"], "memory");
        assert_eq!(first["shown"], false);
        assert_eq!(history::read(&b).unwrap().messages, b_messages);
        assert_eq!(messages::last_text().as_deref(), Some("B current reply"));
        let retry = verified(&runtime, 4, "123", 'a')
            .scope(native_call(&runtime, name, arguments))
            .await
            .unwrap();
        assert_eq!(retry["duplicate"], true);
        assert_eq!(first["message_id"], retry["message_id"]);

        // Use the frames emitted by the real renderer in the native dispatch
        // above. Feed their trusted metadata into the attached State boundary.
        let page = capture.read_for_test().await;
        assert_eq!(page["active_history_id"], b.as_str());
        let mut attached = super::super::State::new(30, 100, &Ui::default());
        attached.follow_attach_history(DisplayHistory::Local(
            HistoryId::parse(page["active_history_id"].as_str().unwrap()).unwrap(),
        ));
        attached.events.set_scope(EventScope::Conversation);
        let first_id = first["message_id"].as_str().unwrap();
        let captured = page["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|frame| {
                let id = frame["value"]["message_id"].as_str();
                id == Some(first_id) || id == Some(b_id.as_str())
            })
            .collect::<Vec<_>>();
        assert_eq!(captured.len(), 2);
        for frame in captured {
            let (label, value, source) = attach::recorded_event(frame);
            let expected = if value["message_id"] == first_id {
                &a
            } else {
                &b
            };
            assert_eq!(source.history.local_id().unwrap().as_str(), expected);
            assert!(source.call.is_none());
            let bytes = crate::client::render::render_event_for_test(label, value, 100).unwrap();
            attached.log_scoped_event(label, value, &bytes, source.clone());
            // The same live projection can be delivered twice.
            attached.log_scoped_event(label, value, &bytes, source);
        }
        for (owner, id) in [(&b, b_id.as_str()), (&a, first_id)] {
            attached.follow_attach_history(DisplayHistory::Local(HistoryId::parse(owner).unwrap()));
            let visible = attached.events.visible_ids();
            let cards = attached
                .events
                .events
                .iter()
                .filter(|event| visible.contains(&event.id))
                .collect::<Vec<_>>();
            assert_eq!(cards.len(), 1);
            assert_eq!(cards[0].meta.message_id.as_deref(), Some(id));
            let rows = attached
                .events
                .render(100, 20, &attached.ui, false)
                .join("\n");
            let (shown, hidden) = if owner == &b {
                ("B current reply", "A late reply")
            } else {
                ("A late reply", "B current reply")
            };
            assert!(rows.contains(shown), "{rows}");
            assert!(!rows.contains(hidden), "{rows}");
        }
        attached.events.set_scope(EventScope::All);
        assert_eq!(attached.events.visible_ids().len(), 2);

        // A and B retain independent verified sources concurrently.
        let (extra_a, extra_b) = tokio::join!(
            late.scope(native_call(
                &runtime,
                "ssmcp.ui.message",
                json!({"text":"A concurrent"})
            )),
            second.scope(native_call(
                &runtime,
                "ssmcp.ui.message",
                json!({"text":"B concurrent"})
            )),
        );
        assert_eq!(extra_a.unwrap()["shown"], false);
        assert_eq!(extra_b.unwrap()["shown"], true);
        let actual_a = history::read(&a).unwrap().messages;
        let actual_b = history::read(&b).unwrap().messages;
        assert_eq!(actual_a.len(), 3);
        assert_eq!(actual_b.len(), 3);
        assert_eq!(actual_a[1].id, first["message_id"].as_str().unwrap());
        assert!(actual_a[1].content.contains("A late reply"));
        assert_eq!(actual_a[2].content, "A concurrent");
        assert_eq!(actual_b[1].id, b_id);
        assert_eq!(actual_b[2].content, "B concurrent");

        // Releasing the live session invalidates earlier verified bindings.
        history::release_current().unwrap();
        let stale = verified(&runtime, 5, "123", 'a')
            .scope(native_call(
                &runtime,
                "ssmcp.ui.message",
                json!({"text":"stale reply"}),
            ))
            .await
            .unwrap_err();
        assert!(stale.contains("history_owner_released"), "{stale}");
        assert_eq!(history::read(&a).unwrap().messages, actual_a);
        assert_eq!(history::read(&b).unwrap().messages, actual_b);
    })
    .await;
    runtime.shutdown().await;
}

#[tokio::test]
async fn verified_ui_message_retries_stay_with_the_original_memory_session() {
    verified_reply_routing(
        "ssmcp.ui.message",
        json!({"text":"A late reply","idempotency_key":"late_message"}),
    )
    .await;
}

#[tokio::test]
async fn verified_ui_present_retries_stay_with_the_original_memory_session() {
    verified_reply_routing(
        "ssmcp.ui.present",
        json!({"title":"A late reply","blocks":[{"type":"text","text":"A presentation fixture"}],
            "idempotency_key":"late_presentation"}),
    )
    .await;
}

#[tokio::test]
async fn rendered_messages_keep_distinct_ids_in_the_live_projection() {
    use crate::{
        client::{interface::history, render},
        session::attach,
    };

    let _serial = HISTORY_FIXTURES.lock().await;
    let (root, _, runtime) = runtime().await;
    history::with_test_history(root.0.join("message-id-history"), async {
        let capture = attach::Handle::capture_for_test();
        let text = "Repeated harness status";
        verified(&runtime, 1, "123", 'a')
            .scope(async {
                render::message(text);
                render::message(text);
            })
            .await;

        let saved = history::current().unwrap().unwrap();
        assert_eq!(saved.messages.len(), 2);
        assert_ne!(saved.messages[0].id, saved.messages[1].id);
        let owner = HistoryId::parse(&saved.metadata.id).unwrap();
        let page = capture.read_for_test().await;
        assert_eq!(page["active_history_id"], owner.as_str());
        let frames: Vec<_> = page["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|frame| frame["local_history_id"] == owner.as_str())
            .collect();
        assert_eq!(frames.len(), saved.messages.len());

        let mut view = EventView::new();
        view.set_history_owner(Some(owner.clone()));
        view.set_scope(EventScope::Conversation);
        for (frame, message) in frames.into_iter().zip(&saved.messages) {
            let (label, value, source) = attach::recorded_event(frame);
            assert_eq!(label, "agent");
            assert_eq!(message.role, "assistant");
            assert_eq!(message.content, text);
            assert_eq!(value["text"], text);
            assert_eq!(value["message_id"].as_str(), Some(message.id.as_str()));
            view.ingest_scoped_event(label, value, b"", source.clone());
            view.ingest_scoped_event(label, value, b"", source);
        }

        let visible = view.visible_ids();
        let cards: Vec<_> = view
            .events
            .iter()
            .filter(|event| visible.contains(&event.id))
            .map(|event| {
                assert!(event.text.contains(text));
                event.meta.message_id.as_deref().unwrap()
            })
            .collect();
        let expected: Vec<_> = saved
            .messages
            .iter()
            .map(|message| message.id.as_str())
            .collect();
        assert_eq!(cards, expected);
    })
    .await;
    runtime.shutdown().await;
}

#[tokio::test]
async fn history_capture_is_lazy_and_headless_first_message_uses_its_reserved_owner() {
    use crate::client::interface::history;
    use crate::runtime::call_scope::{Source, current_source};

    let _serial = HISTORY_FIXTURES.lock().await;
    let (root, _, runtime) = runtime().await;
    let path = root.0.join("headless-history");
    history::with_test_history(path.clone(), async {
        let first_call = verified(&runtime, 1, "123", 'a');
        let reserved = first_call
            .scope(async { current_source().require_owner().unwrap().clone() })
            .await;
        let read = first_call.scope(native_call(&runtime, "ssmcp.exec.list", json!({})));
        // A read-only native observation creates no session record or directory.
        assert_eq!(read.await.unwrap()["jobs"], json!([]));
        assert!(!path.exists());
        assert!(history::sessions().unwrap().is_empty());
        assert!(history::current().unwrap().is_none());
        let nested = first_call
            .scope(async {
                let captured = Source::capture();
                captured
                    .scope(async { Source::capture().require_owner().unwrap().clone() })
                    .await
            })
            .await;
        assert_eq!(nested, reserved);

        let first = first_call
            .scope(native_call(
                &runtime,
                "ssmcp.ui.message",
                json!({"text":"Headless first reply"}),
            ))
            .await
            .unwrap();
        assert_eq!(first["recorded"], true);
        assert_eq!(first["storage"], "memory");
        assert_eq!(first["terminal_active"], false);
        let conversation = history::current().unwrap().unwrap();
        assert_eq!(conversation.metadata.id, reserved.id());
        assert_eq!(conversation.messages.len(), 1);
        assert_eq!(conversation.messages[0].content, "Headless first reply");
        history::release_current().unwrap();
        let stale = first_call
            .scope(native_call(
                &runtime,
                "ssmcp.ui.message",
                json!({"text":"Must not reappear after lease release"}),
            ))
            .await
            .unwrap_err();
        assert!(stale.contains("history_owner_released"), "{stale}");
        assert_eq!(history::sessions().unwrap().len(), 1);
        assert_eq!(
            history::read(reserved.id()).unwrap().messages,
            conversation.messages
        );
    })
    .await;
    runtime.shutdown().await;
}
