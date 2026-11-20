use super::super::{EventScope, Ui, unstyled};
use super::*;
use crate::{
    backend::Backend,
    client::interface::history,
    output::OutputStore,
    runtime::Runtime,
    wire::{CodeContext, ControlAccount, ControlAgent, ControlAttribution},
};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};
use unicode_width::UnicodeWidthStr;

static NEXT: AtomicU64 = AtomicU64::new(1);

struct Fixture {
    root: PathBuf,
    _backend: Arc<Backend>,
    runtime: Arc<Runtime>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
impl Fixture {
    async fn new() -> Self {
        let root = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "ssmcp-tool-summary-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
        ));
        std::fs::create_dir(&root).unwrap();
        let output = OutputStore::at_for_test(root.join("output")).unwrap();
        let backend = Backend::with_output(Path::new("/bin/sh"), root.clone(), output)
            .await
            .unwrap();
        let runtime = Runtime::new(backend.clone(), root.clone()).unwrap();
        Self {
            root,
            _backend: backend,
            runtime,
        }
    }
}

async fn source(
    runtime: &Runtime,
    request: u64,
    subject: &str,
    agent: Option<char>,
    login: &str,
) -> Source {
    let thread = agent.map(|agent| format!("agent_{}", agent.to_string().repeat(32)));
    runtime
        .call_scope(
            request,
            Some(&ControlAttribution {
                account: ControlAccount {
                    provider: "github".into(),
                    subject: subject.into(),
                    login: login.into(),
                },
                owner: None,
                agent: thread.as_ref().map(|id| ControlAgent {
                    id: id.clone(),
                    name: "same visible label".into(),
                }),
            }),
            Some(&CodeContext {
                run_id: format!("{request:032x}"),
                code: "summary native fixture".into(),
                timestamp_ms: 1,
                session_id: thread.as_ref().map(|_| "controlled_client_fixture".into()),
                thread_id: thread,
            }),
        )
        .unwrap()
        .scope(async { Source::capture() })
        .await
}
fn new_view(source: &Source) -> EventView {
    let mut view = EventView::new();
    view.set_history_owner(Some(source.require_owner().unwrap().display_id()));
    view.set_scope(EventScope::Conversation);
    view
}
async fn stat(view: &mut EventView, runtime: &Runtime, request: u64) -> Source {
    let source = source(runtime, request, "123", None, "same-label").await;
    // These real controller frames are emitted around every client request.
    view.ingest_event(
        "controller",
        &json!({"type":"ssmcp.controller.started","request_id":request}),
        b"",
    );
    view.summary_call_started(&source, "ssmcp.fs.stat");
    view.summary_call_finished(&source, true);
    view.ingest_event(
        "controller",
        &json!({"type":"ssmcp.controller.finished","request_id":request}),
        b"",
    );
    source
}
fn group_id(view: &EventView, source: &Source) -> String {
    view.tool_summary_list(source).unwrap()["groups"][0]["group_id"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[tokio::test]
async fn eleventh_real_call_reminds_once_and_summary_restarts_the_counter() {
    let fixture = Fixture::new().await;
    history::with_test_history(fixture.root.join("history"), async {
        let first = source(&fixture.runtime, 1, "123", None, "same-label").await;
        let mut view = new_view(&first);
        for request in 1..=10 {
            let source = stat(&mut view, &fixture.runtime, request).await;
            assert!(view.take_tool_summary_reminder(&source).is_none());
        }
        let eleventh = stat(&mut view, &fixture.runtime, 11).await;
        let reminder = view.take_tool_summary_reminder(&eleventh).unwrap();
        assert_eq!(reminder["call_count"], 11);
        assert_eq!(reminder["revision"], 0);
        assert_eq!(reminder["threshold"], 10);
        assert!(view.take_tool_summary_reminder(&eleventh).is_none());
        let id = reminder["group_id"].as_str().unwrap();
        let updated = view
            .tool_summary_set(&eleventh, id, "Inspecting workspace files", Some(0))
            .unwrap();
        assert_eq!(updated["revision"], 1);
        assert_eq!(updated["covered_call_count"], 11);
        assert_eq!(updated["needs_summary"], false);
        assert!(
            view.tool_summary_set(&eleventh, id, "stale", Some(0))
                .is_err()
        );

        // A fresh outer run has the same authorization scope and can update.
        let next_run = source(&fixture.runtime, 100, "123", None, "renamed login").await;
        assert_eq!(group_id(&view, &next_run), id);
        let duplicate = view
            .tool_summary_set(&next_run, id, "Inspecting workspace files", Some(1))
            .unwrap();
        assert_eq!(duplicate["duplicate"], true);
        assert_eq!(duplicate["revision"], 1);
        for name in [
            "ssmcp.ui.tool_summary",
            "ssmcp.ui.get",
            "ssmcp.client.capabilities",
        ] {
            view.summary_call_started(&next_run, name);
        }
        // A failed summary call stays an error for its caller without inserting
        // a card or splitting the same group's next native dispatch.
        view.ingest_scoped_event(
            "ssmcp",
            &json!({"type":"ssmcp.status","level":"Error","message":"stale summary revision"}),
            b"[ERR] stale summary revision\n",
            next_run.display(),
        );
        assert_eq!(view.events.len(), 11);
        for request in 12..=21 {
            let source = stat(&mut view, &fixture.runtime, request).await;
            assert!(view.take_tool_summary_reminder(&source).is_none());
        }
        let last = stat(&mut view, &fixture.runtime, 22).await;
        let reminder = view.take_tool_summary_reminder(&last).unwrap();
        assert_eq!(reminder["revision"], 1);
        assert_eq!(reminder["call_count"], 22);
        assert_eq!(reminder["covered_call_count"], 11);
        assert!(view.take_tool_summary_reminder(&last).is_none());
        assert_eq!(view.events.len(), 22);
        assert_eq!(view.tool_groups().len(), 1);
        assert_eq!(view.snapshot()["tool_groups"][0]["call_count"], 22);
    })
    .await;
}

#[tokio::test]
async fn lifecycle_duplicates_do_not_count_but_new_poll_requests_do() {
    let fixture = Fixture::new().await;
    history::with_test_history(fixture.root.join("history"), async {
        let call = source(&fixture.runtime, 1, "123", None, "same-label").await;
        let mut view = new_view(&call);
        view.summary_call_started(&call, "ssmcp.exec.start");
        view.summary_call_started(&call, "ssmcp.exec.start");
        // Dispatch completion can win the race with the job's first projection.
        view.summary_call_finished(&call, true);
        assert_eq!(view.events[0].meta.status, Some(Status::Done));
        assert!(view.tool_groups().is_empty());
        assert_eq!(
            view.tool_summary_list(&call).unwrap()["groups"][0]["shown"],
            false
        );
        let started =
            json!({"type":"exec.started","job_id":"summary-job","command":["printf","hello"]});
        view.ingest_scoped_event("shell", &started, b"[RUN] printf hello\n", call.display());
        for _ in 0..12 {
            view.ingest_scoped_event(
                "shell",
                &json!({"type":"exec.stdout","job_id":"summary-job"}),
                b"hello\n",
                call.display(),
            );
        }
        view.summary_call_finished(&call, true);
        assert_eq!(view.events.len(), 1);
        assert_eq!(view.events[0].meta.status, Some(Status::Running));
        assert_eq!(
            view.tool_summary_list(&call).unwrap()["groups"][0]["call_count"],
            1
        );
        view.ingest_scoped_event(
            "shell",
            &json!({"type":"exec.exited","job_id":"summary-job","exit_code":0}),
            b"[OK] exit=0\n",
            call.display(),
        );
        assert_eq!(view.events[0].meta.status, Some(Status::Exit(0)));
        let id = group_id(&view, &call);
        assert_eq!(
            view.tool_summary_set(&call, &id, "Read process output", Some(0))
                .unwrap()["shown"],
            true
        );
        assert_eq!(view.tool_groups().len(), 1);
        for (request, name) in [
            (2, "ssmcp.exec.status"),
            (3, "ssmcp.exec.read"),
            (4, "ssmcp.exec.wait"),
        ] {
            let poll = source(&fixture.runtime, request, "123", None, "same-label").await;
            view.summary_call_started(&poll, name);
            view.summary_call_finished(&poll, true);
        }
        assert_eq!(view.events.len(), 4);
        assert_eq!(view.snapshot()["tool_groups"][0]["call_count"], 4);
        assert!(view.take_tool_summary_reminder(&call).is_none());
    })
    .await;
}

#[tokio::test]
async fn exact_account_connection_history_and_binding_authorize_edits() {
    let fixture = Fixture::new().await;
    let other_connection = Fixture::new().await;
    history::with_test_history(fixture.root.join("history"), async {
        history::record_request("a", "First local conversation").unwrap();
        let first = source(&fixture.runtime, 1, "123", None, "same-label").await;
        let mut view = new_view(&first);
        view.summary_call_started(&first, "ssmcp.fs.stat");
        let id = group_id(&view, &first);
        for other in [
            source(&fixture.runtime, 2, "456", None, "same-label").await,
            source(&other_connection.runtime, 3, "123", None, "same-label").await,
            source(&fixture.runtime, 4, "123", Some('a'), "same-label").await,
        ] {
            assert!(
                view.tool_summary_list(&other).unwrap()["groups"]
                    .as_array()
                    .unwrap()
                    .is_empty()
            );
            assert!(
                view.tool_summary_set(&other, &id, "unauthorized", None)
                    .is_err()
            );
        }
        // An unrelated pane selection never changes the captured owner's access.
        history::release_current().unwrap();
        history::record_request("b", "Second local conversation").unwrap();
        let other_history = source(&fixture.runtime, 5, "123", None, "same-label").await;
        view.set_history_owner(Some(other_history.require_owner().unwrap().display_id()));
        assert!(
            view.tool_summary_set(&other_history, &id, "wrong history", None)
                .is_err()
        );
        let receipt = view
            .tool_summary_set(&first, &id, "First conversation only", Some(0))
            .unwrap();
        assert_eq!(receipt["shown"], false);
        assert_eq!(receipt["accepted"], true);

        let bound = source(&fixture.runtime, 6, "123", Some('b'), "same-label").await;
        view.summary_call_started(&bound, "ssmcp.fs.stat");
        let bound_id = group_id(&view, &bound);
        let same_binding = source(&fixture.runtime, 7, "123", Some('b'), "renamed").await;
        assert!(
            view.tool_summary_set(&same_binding, &bound_id, "Bound worker", Some(0))
                .is_ok()
        );
        let other_binding = source(&fixture.runtime, 8, "123", Some('c'), "renamed").await;
        assert!(
            view.tool_summary_set(&other_binding, &bound_id, "wrong worker", None)
                .is_err()
        );
        assert!(
            view.tool_summary_set(&other_history, &bound_id, "unbound", None)
                .is_err()
        );
    })
    .await;
}

#[tokio::test]
async fn updating_summary_preserves_fold_scroll_and_fits_both_widths() {
    let fixture = Fixture::new().await;
    history::with_test_history(fixture.root.join("history"), async {
        let first = source(&fixture.runtime, 1, "123", None, "same-label").await;
        let mut view = new_view(&first);
        for request in 1..=22 {
            stat(&mut view, &fixture.runtime, request).await;
        }
        let id = group_id(&view, &first);
        let group = view.tool_groups()[0].clone();
        assert!(view.set_tool_expanded(&group, true));
        let ui = Ui {
            color: false,
            ..Ui::default()
        };
        view.render(120, 24, &ui, true);
        view.events[group.start].group_panel.scroll = Some(3);
        view.scroll = 1;
        view.follow = false;
        view.reveal = false;
        let before = (
            view.events.len(),
            view.selected,
            view.selected_group,
            view.scroll,
            view.events[group.start].group_panel.scroll,
        );
        view.tool_summary_set(&first, &id, "Inspecting workspace files", Some(0))
            .unwrap();
        assert_eq!(
            before,
            (
                view.events.len(),
                view.selected,
                view.selected_group,
                view.scroll,
                view.events[group.start].group_panel.scroll
            )
        );
        assert!(view.tool_groups()[0].expanded);
        for width in [120, 48] {
            let group = view.tool_groups()[0].clone();
            let block = super::super::tool_activity::render(
                &mut view.events,
                &group,
                super::super::tool_activity::Options {
                    width,
                    page: 24,
                    palette: "",
                    ui: &ui,
                    selected: view.selected,
                    selected_group: view.selected_group,
                    focused: true,
                    reveal: false,
                },
            );
            let text = block
                .rows
                .iter()
                .map(|(_, row)| unstyled(row))
                .collect::<Vec<_>>()
                .join("\n");
            assert!(text.contains("Inspecting workspace files"), "{text}");
            assert!(text.contains("22 calls"), "{text}");
            assert!(
                block
                    .rows
                    .iter()
                    .all(|(_, row)| unstyled(row).width() <= width)
            );
        }
        let group = view.tool_groups()[0].clone();
        assert!(view.set_tool_expanded(&group, false));
        view.tool_summary_set(&first, &id, "Verified workspace files", Some(1))
            .unwrap();
        assert!(!view.tool_groups()[0].expanded);
    })
    .await;
}

#[tokio::test]
async fn group_identity_survives_eviction_and_reload_is_read_only() {
    let fixture = Fixture::new().await;
    history::with_test_history(fixture.root.join("history"), async {
        let first = source(&fixture.runtime, 1, "123", None, "same-label").await;
        let mut view = new_view(&first);
        stat(&mut view, &fixture.runtime, 1).await;
        let id = group_id(&view, &first);
        view.tool_summary_set(&first, &id, "Inspecting many files", Some(0))
            .unwrap();
        for request in 2..=(MAX_EVENTS as u64 + 5) {
            stat(&mut view, &fixture.runtime, request).await;
        }
        assert_eq!(view.events.len(), MAX_EVENTS);
        assert_eq!(group_id(&view, &first), id);
        assert_eq!(
            view.snapshot()["tool_groups"][0]["call_count"],
            MAX_EVENTS as u64 + 5
        );
        let saved = serde_json::to_value(view.reload_display()).unwrap();
        let encoded = serde_json::to_string(&saved).unwrap();
        assert!(!encoded.contains("same-label"));
        assert!(!encoded.contains("\"connection\""));
        let mut restored = new_view(&first);
        restored.restore_reload_display(serde_json::from_value(saved).unwrap());
        assert_eq!(restored.snapshot()["tool_groups"][0]["group_id"], id);
        assert_eq!(
            restored.snapshot()["tool_groups"][0]["summary"],
            "Inspecting many files"
        );
        assert_eq!(
            restored.snapshot()["tool_groups"][0]["summary_editable"],
            false
        );
        assert!(
            restored.tool_summary_list(&first).unwrap()["groups"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert!(
            restored
                .tool_summary_set(&first, &id, "must remain historical", None)
                .is_err()
        );
        assert!(restored.take_tool_summary_reminder(&first).is_none());
        let native = stat(&mut restored, &fixture.runtime, 1000).await;
        assert_ne!(group_id(&restored, &native), id);
    })
    .await;
}

#[tokio::test]
async fn native_read_paths_and_scoped_failures_remain_in_one_logical_group() {
    let fixture = Fixture::new().await;
    history::with_test_history(fixture.root.join("history"), async {
        let first = source(&fixture.runtime, 1, "123", None, "same-label").await;
        let mut view = new_view(&first);
        view.summary_call_started(&first, "ssmcp.fs.read_text");
        view.ingest_scoped_event(
            "workspace",
            &json!({"type":"ssmcp.file.read","path":"src/important.rs"}),
            b"[READ] workspace src/important.rs\n",
            first.display(),
        );
        view.summary_call_finished(&first, true);
        assert!(view.events[0].chat_title().contains("src/important.rs"));
        assert!(view.events[0].meta.read.is_some());
        for request in 2..=11 {
            let failed = source(&fixture.runtime, request, "123", None, "same-label").await;
            view.summary_call_started(&failed, "ssmcp.fs.stat");
            view.summary_call_finished(&failed, false);
            view.ingest_scoped_event(
                "ssmcp",
                &json!({"type":"ssmcp.status","level":"Error","message":"missing path"}),
                b"[ERR] missing path\n",
                failed.display(),
            );
        }
        assert_eq!(view.events.len(), 11);
        assert_eq!(view.tool_groups().len(), 1);
        assert_eq!(view.snapshot()["tool_groups"][0]["call_count"], 11);
        assert_eq!(view.snapshot()["tool_groups"][0]["stats"]["failed"], 10);
        assert_eq!(
            view.take_tool_summary_reminder(&first).unwrap()["call_count"],
            11
        );
    })
    .await;
}

#[tokio::test]
async fn list_returns_only_the_latest_thirty_two_authorized_groups() {
    let fixture = Fixture::new().await;
    history::with_test_history(fixture.root.join("history"), async {
        let first = source(&fixture.runtime, 1, "123", None, "same-label").await;
        let mut view = new_view(&first);
        let mut ids = Vec::new();
        for request in 1..=40 {
            let call = stat(&mut view, &fixture.runtime, request).await;
            let list = view.tool_summary_list(&call).unwrap();
            ids.push(list["groups"].as_array().unwrap().last().unwrap()["group_id"].clone());
            view.ingest_scoped_event(
                "message",
                &json!({"type":"ssmcp.message","text":"Next work item"}),
                b"[AGENT] Next work item\n",
                call.display(),
            );
        }
        let list = view.tool_summary_list(&first).unwrap();
        let groups = list["groups"].as_array().unwrap();
        assert_eq!(groups.len(), 32);
        assert_eq!(list["truncated"], true);
        assert_eq!(list["has_more"], true);
        assert_eq!(groups[0]["group_id"], ids[8]);
        assert_eq!(groups[31]["group_id"], ids[39]);
    })
    .await;
}

#[tokio::test]
async fn ui_mutations_keep_non_conversation_image_question_and_task_events() {
    let fixture = Fixture::new().await;
    history::with_test_history(fixture.root.join("history"), async {
        let first = source(&fixture.runtime, 1, "123", None, "same-label").await;
        let mut view = new_view(&first);
        for (request, name, kind) in [
            (1, "ssmcp.ui.present", "image"),
            (2, "ssmcp.ui.ask", "ssmcp.question"),
            (3, "ssmcp.ui.harness", "ssmcp.harness.task"),
        ] {
            let call = source(&fixture.runtime, request, "123", None, "same-label").await;
            view.summary_call_started(&call, name);
            view.ingest_scoped_event(
                "native",
                &json!({"type":kind}),
                b"Visible UI payload\n",
                call.display(),
            );
            assert_eq!(view.events.len(), request as usize);
            assert_ne!(view.events.back().unwrap().meta.kind, Kind::Conversation);
        }
        assert!(
            view.tool_summary_list(&first).unwrap()["groups"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    })
    .await;
}

#[test]
fn summary_text_is_bounded_single_line_and_redacted() {
    for bad in [
        "",
        "   ",
        "two\nlines",
        "tab\there",
        "\u{1b}[31mred",
        "break\u{2028}line",
    ] {
        assert!(clean_summary(bad).is_err(), "{bad:?}");
    }
    assert!(clean_summary(&"界".repeat(120)).is_ok());
    assert!(clean_summary(&"界".repeat(121)).is_err());
    assert!(clean_summary(&"🙂".repeat(120)).is_ok());
    let safe = clean_summary("Checking API_TOKEN=private-summary-token").unwrap();
    assert!(!safe.contains("private-summary-token"));
    assert!(safe.chars().count() <= MAX_SUMMARY_CHARS);
}
