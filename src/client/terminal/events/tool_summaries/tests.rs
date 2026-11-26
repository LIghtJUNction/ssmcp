use super::super::{EventScope, Ui, unstyled};
use super::*;
use crate::{
    backend::Backend,
    client::interface::history,
    output::OutputStore,
    runtime::Runtime,
    wire::{CodeContext, ControlAccount, ControlAgent, ControlAttribution, Operation},
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
    view.summary_call_started(&source, "ssmcp.fs.stat", false);
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

async fn command_call(view: &mut EventView, runtime: &Runtime, request: u64) -> Source {
    let source = source(runtime, request, "123", None, "same-label").await;
    let job = format!("summary-command-{request}");
    view.summary_call_started(&source, "ssmcp.exec.start", false);
    view.ingest_scoped_event(
        "shell",
        &json!({"type":"exec.started","job_id":job,"command":["printf",format!("command-{request}")]}),
        b"[RUN] command\n", source.display(),
    );
    view.ingest_scoped_event(
        "shell",
        &json!({"type":"exec.exited","job_id":job,"exit_code":0}),
        b"[OK] exit=0\n",
        source.display(),
    );
    view.summary_call_finished(&source, true);
    source
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
        let next_run = source(&fixture.runtime, 100, "123", None, "renamed-login").await;
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
            view.summary_call_started(&next_run, name, false);
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
async fn oversized_native_reply_restores_only_its_original_summary_reservation() {
    let fixture = Fixture::new().await;
    let other_connection = Fixture::new().await;
    history::with_test_history(fixture.root.join("history"), async {
        let first = source(&fixture.runtime, 1, "123", None, "same-label").await;
        let mut view = new_view(&first);
        for request in 1..=10 {
            stat(&mut view, &fixture.runtime, request).await;
        }
        let eleventh = stat(&mut view, &fixture.runtime, 11).await;
        let reminder = view.take_tool_summary_reminder(&eleventh).unwrap();
        let id = reminder["group_id"].as_str().unwrap().to_owned();
        assert!(view.take_tool_summary_reminder(&eleventh).is_none());
        let oversized = crate::client::interface::tool_summary::attach_metadata(
            json!({"content":[{"type":"text","text":"x".repeat(crate::wire::MAX_FRAME)}]}),
            Some(reminder.clone()),
        );
        let rejected = crate::client::bounded_native_reply(Ok(oversized), |notice| {
            assert!(view.restore_tool_summary_reminder(&eleventh, notice));
        });
        assert!(rejected.unwrap_err().contains("4 MiB bridge frame limit"));

        // Recover by observing state, not by dispatching the original business
        // operation again. The real guard above is the only overflow boundary.
        let probe = source(&fixture.runtime, 12, "123", None, "same-label").await;
        view.summary_call_started(&probe, "ssmcp.ui.tool_summary", false);
        let body = view.tool_summary_list(&probe).unwrap();
        assert_eq!(body["groups"][0]["call_count"], 11);
        assert_eq!(body["groups"][0]["revision"], 0);
        assert_eq!(view.events.len(), 11);
        let retry = view.take_tool_summary_reminder(&probe).unwrap();
        assert_eq!(retry, reminder);
        let reply = crate::client::interface::tool_summary::attach_metadata(
            crate::backend::result(body),
            Some(retry),
        );
        let delivered = crate::client::bounded_native_reply(Ok(reply), |_| {
            panic!("a bounded summary reply must keep its reservation")
        })
        .unwrap();
        #[cfg(target_os = "linux")]
        {
            let mut captured = crate::code_mode::tool_summaries::Capture::default();
            captured.reconcile("fixture", None, &delivered);
            captured.observe("fixture", None, &delivered);
            let outer = crate::code_mode::tool_summaries::append(
                json!({"content":[],"structuredContent":{"result":null},"isError":true}),
                captured.for_scope("fixture", None),
            );
            assert_eq!(outer["isError"], true);
            let notices = outer["structuredContent"]["tool_summary_reminders"]
                .as_array()
                .unwrap();
            assert_eq!(notices.len(), 1);
            assert_eq!(notices[0]["group_id"], id);
            assert_eq!(notices[0]["revision"], 0);
        }
        drop(delivered);
        assert!(view.take_tool_summary_reminder(&probe).is_none());

        // A late cancellation cannot rewind the replacement reply, even when
        // both requests refer to the same group and summary revision.
        assert!(!view.restore_tool_summary_reminder(&eleventh, &reminder));
        for other in [
            source(&fixture.runtime, 12, "456", None, "same-label").await,
            source(&fixture.runtime, 12, "123", Some('a'), "same-label").await,
            source(&other_connection.runtime, 12, "123", None, "same-label").await,
        ] {
            assert!(!view.restore_tool_summary_reminder(&other, &reminder));
        }
        assert!(view.take_tool_summary_reminder(&probe).is_none());

        view.tool_summary_set(&probe, &id, "Inspected the workspace", Some(0))
            .unwrap();
        let mut latest = probe.clone();
        for request in 13..=23 {
            latest = stat(&mut view, &fixture.runtime, request).await;
        }
        let newer = view.take_tool_summary_reminder(&latest).unwrap();
        assert_eq!(newer["revision"], 1);
        assert_eq!(newer["call_count"], 22);
        assert!(!view.restore_tool_summary_reminder(&probe, &reminder));
        assert!(view.take_tool_summary_reminder(&latest).is_none());
    })
    .await;
}

#[tokio::test]
async fn lifecycle_duplicates_do_not_count_but_new_poll_requests_do() {
    let fixture = Fixture::new().await;
    history::with_test_history(fixture.root.join("history"), async {
        let call = source(&fixture.runtime, 1, "123", None, "same-label").await;
        let mut view = new_view(&call);
        view.summary_call_started(&call, "ssmcp.exec.start", false);
        view.summary_call_started(&call, "ssmcp.exec.start", false);
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
        for width in [120, 48] {
            let ui = Ui {
                color: false,
                ..Ui::default()
            };
            let rows = view.render(width, 24, &ui, true);
            let text = rows.join("\n");
            assert_eq!(text.matches("Read process output").count(), 1, "{text}");
            assert_eq!(text.matches("Ran printf hello").count(), 1, "{text}");
            assert!(text.contains("Show details"), "{text}");
            assert!(
                rows.iter()
                    .all(|row| unstyled(row).width() <= usize::from(width))
            );
        }
        for (request, name) in [
            (2, "ssmcp.exec.status"),
            (3, "ssmcp.exec.read"),
            (4, "ssmcp.exec.wait"),
        ] {
            let poll = source(&fixture.runtime, request, "123", None, "same-label").await;
            view.summary_call_started(&poll, name, false);
            view.summary_call_finished(&poll, true);
        }
        assert_eq!(view.events.len(), 4);
        assert_eq!(view.snapshot()["tool_groups"][0]["call_count"], 4);
        assert!(view.take_tool_summary_reminder(&call).is_none());
    })
    .await;
}

fn read_result(stdout: &str, stderr: &str) -> Value {
    crate::backend::result(json!({
        "job_id":"reader-job", "running":false, "exit_code":0,
        "stdout":stdout, "stderr":stderr,
        "stdout_cursor":stdout.len(), "stderr_cursor":stderr.len(),
        "stdout_eof":true, "stderr_eof":true,
        "stdout_truncated":false, "stderr_truncated":false,
        "stdout_base64":STANDARD.encode(stdout.as_bytes()),
        "stderr_base64":STANDARD.encode(stderr.as_bytes()),
        "snapshot":{
            "command":["private-command-must-not-be-shown"],
            "cwd":"/private/cwd-must-not-be-shown",
            "stdout":{"artifact":{"id":"stdout-artifact", "path":"/private/stdout"}},
            "stderr":{"artifact":{"id":"stderr-artifact", "path":"/private/stderr"}}
        }
    }))
}

fn expand_exec_read_group(view: &mut EventView, call: &Source) {
    let group = group_id(view, call);
    view.tool_summary_set(call, &group, "Inspect command output", Some(0))
        .unwrap();
    let group = view.tool_groups()[0].clone();
    view.set_tool_expanded(&group, true);
    view.selected_group = None;
    view.selected = Some(view.events[0].id);
}

fn export_exec_read_fixture(view: &mut EventView, phase: &str, at_end: bool) {
    let Some(root) = std::env::var_os("SSMCP_EXEC_READ_FIXTURE") else {
        return;
    };
    let root = PathBuf::from(root);
    std::fs::create_dir_all(&root).unwrap();
    let ui = Ui {
        color: true,
        ..Ui::default()
    };
    let position = view.position();
    for width in [120, 48] {
        view.restore_position(&position);
        // Reflow before scrolling so each fixture uses its own viewport bounds.
        view.render(width, 29, &ui, true);
        if at_end {
            assert!(view.scroll_selected_panel(isize::MAX));
        }
        let rows = view.render(width, 29, &ui, true);
        let name = if width == 120 {
            phase.to_owned()
        } else {
            format!("{phase}-{width}")
        };
        let label = super::super::clip_plain(
            &format!("SSMCP exec.read renderer fixture: {phase}"),
            usize::from(width),
        );
        // Reserve one row for the label in each actual width x 30 capture.
        std::fs::write(
            root.join(format!("{name}.ansi")),
            format!("\x1b[2J\x1b[H{label}\r\n{}\x1b[0m", rows.join("\r\n")),
        )
        .unwrap();
        std::fs::write(
            root.join(format!("{name}.json")),
            serde_json::to_vec(&json!({"cols":width,"rows":30})).unwrap(),
        )
        .unwrap();
    }
    view.restore_position(&position);
}

#[tokio::test]
async fn exec_read_native_result_expands_without_replaying_folded_output() {
    let fixture = Fixture::new().await;
    history::with_test_history(fixture.root.join("history"), async {
        let call = source(&fixture.runtime, 1, "123", None, "same-label").await;
        let mut view = new_view(&call);
        let job = fixture.runtime.perform(Operation::CallTool {
            name: "ssmcp.exec.start".into(),
            arguments: json!({"command":["/bin/sh", "-c", "printf 'reader-stdout\\nAPI_TOKEN=fixture-token-value\\n'; printf 'reader-stderr\\n' >&2"]}),
        }).await.unwrap();
        let job_id = &job["structuredContent"]["job_id"];
        let finished = fixture.runtime.perform(Operation::CallTool {
            name: "ssmcp.exec.wait".into(),
            arguments: json!({"job_id":job_id,"wait_ms":5000}),
        }).await.unwrap();
        assert_eq!(finished["structuredContent"]["running"], false);
        view.summary_call_started(&call, "ssmcp.exec.read", false);
        let id = view.events[0].id;
        // Populate the local projection with the actual native MCP envelope.
        let result = call.clone().scope(fixture.runtime.perform(Operation::CallTool {
            name: "ssmcp.exec.read".into(),
            arguments: json!({"job_id":job_id}),
        })).await.unwrap();
        let original = result.clone();
        assert!(result["structuredContent"]["stdout"].as_str().unwrap().contains("fixture-token-value"));
        assert!(view.summary_exec_read_result(&call, &result));
        view.summary_call_finished(&call, true);
        assert_eq!(result, original);
        assert_eq!(view.events.len(), 1);
        assert_eq!(view.events[0].id, id);
        assert!(view.events[0].meta.keys.is_empty());
        assert_eq!(view.events[0].meta.status, Some(Status::Done));
        // Keyed observations keep their own renderer even if their retained
        // text looks exactly like a native exec.read receipt.
        for key in ["wait-observation\0exec.read", "worker-operation\0exec.read"] {
            view.events[0].meta.keys = vec![key.to_owned()];
            assert!(exec_read_preview(&view.events[0], 48).is_none());
        }
        view.events[0].meta.keys.clear();
        let ui = Ui { color: false, ..Ui::default() };
        for width in [48, 120] {
            let folded = view.render(width, 24, &ui, true).join("\n");
            assert!(folded.contains("ssmcp.exec.read"), "{folded}");
            assert!(folded.contains("stdout 44 B"), "{folded}");
            if width == 120 {
                assert!(folded.contains("stderr 14 B"), "{folded}");
            }
            for raw in ["cursor=", "artifact=", "truncated=", "eof="] {
                assert!(!folded.contains(raw), "{folded}");
            }
            assert!(!folded.contains("reader-stdout"), "{folded}");
            assert!(!folded.contains("reader-stderr"), "{folded}");
            assert!(!folded.contains("fixture-token-value"), "{folded}");
        }
        assert!(view.open());
        let expanded = view.render(120, 30, &ui, true).join("\n");
        assert!(expanded.contains("reader-stdout"), "{expanded}");
        assert!(expanded.contains("reader-stderr"), "{expanded}");
        assert!(expanded.contains("eof=true truncated=false"), "{expanded}");
        assert!(expanded.contains("API_TOKEN=[redacted]"), "{expanded}");
        assert!(!expanded.contains("fixture-token-value"), "{expanded}");
        assert!(view.back());
        let folded = view.render(120, 30, &ui, true).join("\n");
        assert!(!folded.contains("reader-stdout"), "{folded}");

        // The same details and one-line projection also work inside a tool window.
        expand_exec_read_group(&mut view, &call);
        let folded = view.render(120, 30, &ui, true).join("\n");
        assert!(folded.contains("stdout 44 B"), "{folded}");
        assert!(!folded.contains("reader-stdout"), "{folded}");
        assert!(!folded.contains("reader-stderr"), "{folded}");
        export_exec_read_fixture(&mut view, "collapsed", false);
        assert!(view.open());
        for width in [48, 120] {
            let expanded = view.render(width, 30, &ui, true).join("\n");
            assert!(expanded.contains("reader-stdout"), "{expanded}");
            assert!(expanded.contains("reader-stderr"), "{expanded}");
            assert!(expanded.contains("API_TOKEN=[redacted]"), "{expanded}");
            assert!(!expanded.contains("fixture-token-value"), "{expanded}");
            assert!(view.scroll_selected_panel(isize::MAX));
            let tail = view.render(width, 30, &ui, true).join("\n");
            assert!(tail.contains("eof=true"), "{tail}");
            assert!(tail.contains("truncated="), "{tail}");
            assert!(tail.contains("artifact=none"), "{tail}");
            assert!(!tail.contains("fixture-token-value"), "{tail}");
            assert!(view.scroll_selected_panel(isize::MIN + 1));
        }
        export_exec_read_fixture(&mut view, "expanded", false);
        export_exec_read_fixture(&mut view, "expanded-tail", true);
    }).await;
    fixture.runtime.shutdown().await;
}

#[tokio::test]
async fn exec_read_result_uses_current_call_identity_and_replaces_its_own_details() {
    let fixture = Fixture::new().await;
    let other = Fixture::new().await;
    history::with_test_history(fixture.root.join("history"), async {
        let creator = source(&fixture.runtime, 1, "123", None, "same-label").await;
        let reader = source(&fixture.runtime, 2, "123", None, "same-label").await;
        // Same request/display identity on a different live connection.
        let foreign = source(&other.runtime, 2, "123", None, "same-label").await;
        let mut view = new_view(&creator);
        view.summary_call_started(&creator, "ssmcp.exec.start", false);
        view.summary_call_started(&reader, "ssmcp.exec.read", false);
        let reader_id = view.events[1].id;
        view.summary_call_started(&foreign, "ssmcp.exec.read", false);
        let selected = view.selected;
        let original = read_result("only-current-reader\n", "");
        assert!(!view.summary_exec_read_result(&creator, &original));
        assert!(view.summary_exec_read_result(&reader, &original));
        assert_eq!(view.events.len(), 3);
        assert_eq!(view.events[1].id, reader_id);
        assert_eq!(view.selected, selected);
        assert!(view.events[1].text.contains("only-current-reader"));
        assert!(!view.events[0].text.contains("only-current-reader"));
        assert!(!view.events[2].text.contains("only-current-reader"));
        assert!(view.events[1].meta.keys.is_empty());
        assert!(view.summary_exec_read_result(&reader, &read_result("new-page\n", "")));
        assert!(!view.events[1].text.contains("only-current-reader"));
        assert_eq!(view.events[1].text.matches("new-page").count(), 1);
        let missing = source(&fixture.runtime, 3, "123", None, "same-label").await;
        assert!(!view.summary_exec_read_result(&missing, &original));
        assert!(!view.summary_exec_read_result(&Source::capture(), &original));
        assert_eq!(view.events.len(), 3);
        assert_eq!(
            view.bytes,
            view.events
                .iter()
                .map(super::super::Event::retained_bytes)
                .sum::<usize>()
                + view
                    .summary_groups
                    .iter()
                    .map(Group::retained_bytes)
                    .sum::<usize>()
        );
    })
    .await;
}

#[test]
fn exec_read_details_are_redacted_bounded_and_exclude_encoded_and_snapshot_bodies() {
    let output = format!(
        "safe-prefix\nAPI_TOKEN=private-token-value\n{}\nsafe-tail\n",
        "🦀".repeat(40_000)
    );
    let original = read_result(&output, "password=private-password\nordinary-stderr\n");
    let projected = exec_read_detail(&original).unwrap();
    let (detail, truncated) = (projected.text, projected.truncated);
    assert!(truncated);
    assert!(detail.len() <= super::super::MAX_DETAIL);
    assert!(detail.contains("safe-prefix") && detail.contains("safe-tail"));
    assert!(detail.contains("ordinary-stderr"));
    assert!(detail.contains("preview truncated"));
    assert!(detail.contains("artifact=stdout-artifact"));
    assert!(detail.contains("artifact=stderr-artifact"));
    assert!(detail.contains("[redacted]"));
    for forbidden in [
        "private-token-value",
        "private-password",
        "base64",
        "encoded-stdout",
        "encoded-stderr",
        "private-command",
        "/private/",
    ] {
        assert!(!detail.contains(forbidden), "{forbidden}");
    }
    assert!(
        original["structuredContent"]["stdout"]
            .as_str()
            .unwrap()
            .contains("private-token-value")
    );
    assert_eq!(
        original["structuredContent"]["stderr_base64"],
        STANDARD.encode("password=private-password\nordinary-stderr\n")
    );
}

#[test]
fn exec_read_details_keep_empty_stream_state_and_reject_failed_or_unstructured_results() {
    let mut original = read_result("", "");
    original["structuredContent"]["stdout_truncated"] = true.into();
    original["structuredContent"]["stderr_eof"] = false.into();
    let projected = exec_read_detail(&original).unwrap();
    let (detail, truncated) = (projected.text, projected.truncated);
    assert!(!truncated);
    assert!(detail.contains("stdout cursor=0 eof=true truncated=true"));
    assert!(detail.contains("stderr cursor=0 eof=false truncated=false"));
    assert_eq!(detail.matches("(empty)").count(), 2);
    original["isError"] = true.into();
    assert!(exec_read_detail(&original).is_none());
    assert!(
        exec_read_detail(&json!({"content":[{"type":"text", "text":"forged output"}]})).is_none()
    );
    original["isError"] = false.into();
    original["structuredContent"]["stdout_cursor"] = "malformed".into();
    assert!(exec_read_detail(&original).is_none());
}

#[test]
fn exec_read_compact_status_keeps_process_and_output_lifecycles_separate() {
    for (running, exit_code, eof, process, expected, tone) in [
        (true, Value::Null, true, json!({}), "Running", "foreground"),
        (false, json!(0), false, json!({}), "Exit 0", "foreground"),
        (
            false,
            Value::Null,
            true,
            json!({}),
            "Exit unknown",
            "warning",
        ),
        (false, json!(7), true, json!({}), "Exit 7", "error"),
        (
            false,
            Value::Null,
            true,
            json!({"timed_out":true}),
            "Timed out",
            "warning",
        ),
        (
            false,
            Value::Null,
            true,
            json!({"cancelled":true}),
            "Cancelled",
            "warning",
        ),
        (
            false,
            Value::Null,
            true,
            json!({"error":"native process failure"}),
            "Process error",
            "error",
        ),
        (
            false,
            Value::Null,
            true,
            json!({"state":"failed"}),
            "Process error",
            "error",
        ),
        (
            false,
            Value::Null,
            true,
            json!({"exit_signal":15}),
            "Signal 15",
            "error",
        ),
    ] {
        let mut result = read_result("", "");
        let read = &mut result["structuredContent"];
        read["running"] = running.into();
        read["exit_code"] = exit_code;
        read["stdout_eof"] = eof.into();
        read["stderr_eof"] = eof.into();
        read["snapshot"] = process;
        let detail = exec_read_detail(&result).unwrap();
        let output = if eof { "EOF" } else { "output pending" };
        assert_eq!(
            detail.text.lines().nth(1).unwrap(),
            format!("[RESULT] {expected} · {output} · No new output")
        );
        assert_eq!(detail.tone, tone);
    }
}

#[test]
fn exec_read_compact_amounts_count_native_page_bytes_and_keep_unknown_sizes_unknown() {
    let raw = [0xff, 0xfe, 0, b'A'];
    let lossy = String::from_utf8_lossy(&raw);
    assert_ne!(lossy.len(), raw.len());
    let mut result = read_result(&lossy, "猫");
    let read = &mut result["structuredContent"];
    read["stdout_base64"] = STANDARD.encode(raw).into();
    read["stdout_cursor"] = 9_876_543u64.into();
    let detail = exec_read_detail(&result).unwrap();
    let summary = detail.text.lines().nth(1).unwrap();
    assert!(summary.contains("stdout 4 B · stderr 3 B"), "{summary}");
    assert!(!summary.contains("9876543"), "{summary}");
    assert!(detail.text.contains("stdout cursor=9876543"));
    for encoded in [Value::Null, json!("not base64")] {
        result["structuredContent"]["stdout_base64"] = encoded;
        let detail = exec_read_detail(&result).unwrap();
        let summary = detail.text.lines().nth(1).unwrap();
        assert!(summary.contains("stdout read · stderr 3 B"), "{summary}");
        assert!(!summary.contains("stdout 4 B"), "{summary}");
    }
}

#[tokio::test]
async fn exec_read_compact_warnings_survive_narrow_panes_and_detail_retention() {
    let fixture = Fixture::new().await;
    history::with_test_history(fixture.root.join("history"), async {
        let call = source(&fixture.runtime, 1, "123", None, "same-label").await;
        let ui = Ui {
            color: false,
            ..Ui::default()
        };
        for grouped in [false, true] {
            for (exit_code, process, label, tone) in [
                (
                    json!(i32::MIN),
                    json!({"state":"failed","error":"API_TOKEN=private-error-token"}),
                    "Process error",
                    "error",
                ),
                (Value::Null, json!({}), "Exit unknown", "warning"),
                (json!(0), json!({}), "Exit 0", "warning"),
            ] {
                let mut view = new_view(&call);
                view.summary_call_started(&call, "ssmcp.exec.read", false);
                let mut result = read_result("stdout-payload\n", "stderr-payload\n");
                let read = &mut result["structuredContent"];
                read["exit_code"] = exit_code;
                read["stdout_truncated"] = true.into();
                read["stderr_truncated"] = true.into();
                for (key, value) in process.as_object().unwrap() {
                    read["snapshot"][key] = value.clone();
                }
                assert!(view.summary_exec_read_result(&call, &result));
                view.summary_call_finished(&call, true);
                assert_eq!(view.events[0].tone, tone);
                if grouped {
                    expand_exec_read_group(&mut view, &call);
                }
                for width in [32, 48, 120] {
                    let rows = view.render(width, 24, &ui, true);
                    let folded = rows.join("\n");
                    assert!(folded.contains(label), "{folded}");
                    assert!(folded.contains("TRUNCATED"), "{folded}");
                    assert!(
                        rows.iter()
                            .all(|row| unstyled(row).width() <= usize::from(width))
                    );
                    for raw in [
                        "cursor=",
                        "artifact=",
                        "eof=",
                        "truncated=",
                        "job=",
                        "stdout-payload",
                        "stderr-payload",
                        "private-error-token",
                    ] {
                        assert!(!folded.contains(raw), "{folded}");
                    }
                }
                assert!(view.open());
                let mut expanded = view.render(120, 30, &ui, true).join("\n");
                // Short stdout/stderr remain visible on the first group page.
                assert!(expanded.contains("stdout-payload"), "{expanded}");
                assert!(expanded.contains("stderr-payload"), "{expanded}");
                if grouped {
                    assert!(view.scroll_selected_panel(isize::MAX));
                    expanded.push_str(&view.render(120, 30, &ui, true).join("\n"));
                }
                for retained in [
                    "artifact=stdout-artifact",
                    "artifact=stderr-artifact",
                    "truncated=true",
                ] {
                    assert!(expanded.contains(retained), "{expanded}");
                }
                assert!(!expanded.contains("private-error-token"), "{expanded}");
            }

            // Two native-sized pages of invalid UTF-8 exceed text retention
            // after lossy decoding, even when neither native page was lost.
            let raw = vec![0xff; 32768];
            let text = String::from_utf8_lossy(&raw);
            let mut result = read_result(&text, &text);
            for stream in ["stdout", "stderr"] {
                result["structuredContent"][format!("{stream}_base64")] =
                    STANDARD.encode(&raw).into();
                result["structuredContent"][format!("{stream}_cursor")] = (raw.len() as u64).into();
            }
            let mut view = new_view(&call);
            view.summary_call_started(&call, "ssmcp.exec.read", false);
            assert!(view.summary_exec_read_result(&call, &result));
            view.summary_call_finished(&call, true);
            assert!(view.events[0].truncated);
            assert_eq!(view.events[0].tone, "warning");
            let detail = &view.events[0].text;
            assert!(detail.len() <= super::super::MAX_DETAIL);
            for retained in [
                "preview truncated",
                "stdout cursor=32768 eof=true truncated=false artifact=stdout-artifact",
                "stderr cursor=32768 eof=true truncated=false artifact=stderr-artifact",
            ] {
                assert!(detail.contains(retained), "{retained}");
            }
            if grouped {
                expand_exec_read_group(&mut view, &call);
            }
            for width in [32, 48, 120] {
                let rows = view.render(width, 24, &ui, true);
                let folded = rows.join("\n");
                assert!(folded.contains("TRUNCATED details"), "{folded}");
                assert!(
                    rows.iter()
                        .all(|row| unstyled(row).width() <= usize::from(width))
                );
                for raw in ["cursor=", "artifact=", "eof=", "truncated=", "job="] {
                    assert!(!folded.contains(raw), "{folded}");
                }
                // The window consumes only the final expansion hint; it must
                // not consume either of the at-most-two result rows.
                let preview = view.events[0].chat_preview(usize::from(width));
                assert!(preview.len() <= 3);
                assert_eq!(preview.last().unwrap(), "… Click expand");
                assert!(
                    preview[..preview.len() - 1]
                        .join("\n")
                        .contains("TRUNCATED details")
                );
            }
        }
    })
    .await;
    fixture.runtime.shutdown().await;
}

#[tokio::test]
async fn commands_and_other_tools_share_one_visible_summary_and_exact_count() {
    let fixture = Fixture::new().await;
    history::with_test_history(fixture.root.join("history"), async {
        let first = source(&fixture.runtime, 1, "123", None, "same-label").await;
        let mut view = new_view(&first);
        stat(&mut view, &fixture.runtime, 1).await;
        let command = command_call(&mut view, &fixture.runtime, 2).await;
        let read = source(&fixture.runtime, 3, "123", None, "same-label").await;
        view.summary_call_started(&read, "ssmcp.fs.read_text", false);
        view.ingest_scoped_event(
            "workspace",
            &json!({"type":"ssmcp.file.read","path":"src/main.rs"}),
            b"[READ] src/main.rs\n",
            read.display(),
        );
        view.summary_call_finished(&read, true);
        command_call(&mut view, &fixture.runtime, 4).await;
        for _ in 0..3 {
            view.ingest_scoped_event(
                "shell",
                &json!({"type":"exec.stdout","job_id":"summary-command-2"}),
                b"checked\n",
                command.display(),
            );
        }
        view.ingest_scoped_event(
            "shell",
            &json!({"type":"exec.exited","job_id":"summary-command-2","exit_code":0}),
            b"[OK] exit=0\n",
            command.display(),
        );
        let id = group_id(&view, &first);
        let receipt = view
            .tool_summary_set(&first, &id, "Inspect workspace and tools", Some(0))
            .unwrap();
        assert_eq!(receipt["shown"], true);
        assert_eq!(receipt["call_count"], 4);
        let snapshot = view.snapshot();
        assert_eq!(snapshot["tool_groups"].as_array().unwrap().len(), 1);
        assert_eq!(snapshot["tool_groups"][0]["group_id"], id);
        assert_eq!(snapshot["tool_groups"][0]["count"], 4);
        assert_eq!(snapshot["tool_groups"][0]["call_count"], 4);
        assert_eq!(view.events.len(), 4);
        for width in [120, 48] {
            let ui = Ui {
                color: false,
                ..Ui::default()
            };
            for index in [1, 3] {
                view.selected = Some(view.events[index].id);
                view.selected_group = None;
                view.follow = false;
                view.reveal = true;
                let rows = view.render(width, 24, &ui, true);
                let text = rows.join("\n");
                assert_eq!(
                    text.matches("Inspect workspace and tools").count(),
                    1,
                    "{text}"
                );
                assert_eq!(text.matches("4 calls").count(), 1, "{text}");
                assert!(
                    text.contains(&format!("Ran printf command-{}", index + 1)),
                    "{text}"
                );
                assert!(text.contains("Show details"), "{text}");
                assert!(
                    rows.iter()
                        .all(|row| unstyled(row).width() <= usize::from(width))
                );
            }
        }
    })
    .await;
}

#[tokio::test]
async fn exec_reminder_summary_and_retention_never_reopen_an_explicit_fold() {
    let fixture = Fixture::new().await;
    history::with_test_history(fixture.root.join("history"), async {
        let first = source(&fixture.runtime, 1, "123", None, "same-label").await;
        let mut view = new_view(&first);
        for request in 1..=11 {
            let call = command_call(&mut view, &fixture.runtime, request).await;
            let reminder = view.take_tool_summary_reminder(&call);
            if request < 11 {
                assert!(reminder.is_none());
            } else {
                assert_eq!(reminder.unwrap()["call_count"], 11);
                assert!(view.take_tool_summary_reminder(&call).is_none());
            }
        }
        let id = group_id(&view, &first);
        let receipt = view
            .tool_summary_set(&first, &id, "Checked eleven commands", Some(0))
            .unwrap();
        assert_eq!(receipt["shown"], true);
        assert_eq!(receipt["covered_call_count"], 11);
        assert_eq!(view.tool_groups().len(), 1);
        assert!(view.tool_groups()[0].expanded);
        view.select(isize::MIN);
        assert!(view.back());
        let group = view.tool_groups()[0].clone();
        assert!(!view.events[group.start].group_automatic);
        let next = command_call(&mut view, &fixture.runtime, 12).await;
        assert!(view.take_tool_summary_reminder(&next).is_none());
        let receipt = view
            .tool_summary_set(&next, &id, "Checked twelve commands", Some(1))
            .unwrap();
        assert_eq!(receipt["shown"], true);
        assert_eq!(receipt["call_count"], 12);
        assert!(!view.tool_groups()[0].expanded);
        let ui = Ui {
            color: false,
            ..Ui::default()
        };
        let text = view.render(48, 24, &ui, true).join("\n");
        assert_eq!(text.matches("Checked twelve commands").count(), 1, "{text}");
        assert!(!text.contains("printf"), "{text}");
        let saved_canvas = view.position();
        let old_head = view.tool_groups()[0].head;
        for request in 13..=(MAX_EVENTS as u64 + 5) {
            command_call(&mut view, &fixture.runtime, request).await;
        }
        assert_eq!(view.events.len(), MAX_EVENTS);
        assert_eq!(group_id(&view, &first), id);
        assert_eq!(view.tool_groups().len(), 1);
        assert!(!view.tool_groups()[0].expanded);
        assert!(!view.events[view.tool_groups()[0].start].group_automatic);
        assert_eq!(
            view.snapshot()["tool_groups"][0]["call_count"],
            MAX_EVENTS as u64 + 5
        );
        assert_ne!(view.tool_groups()[0].head, old_head);
        view.restore_position(&saved_canvas);
        let text = view.render(48, 24, &ui, true).join("\n");
        assert!(!view.tool_groups()[0].expanded);
        assert!(!view.events[view.tool_groups()[0].start].group_automatic);
        assert_eq!(view.selected_group, Some(view.tool_groups()[0].head));
        assert!(!text.contains("printf"), "{text}");
    })
    .await;
}

#[tokio::test]
async fn inactive_canvas_keeps_group_choice_after_every_saved_command_is_evicted() {
    let fixture = Fixture::new().await;
    history::with_test_history(fixture.root.join("history"), async {
        let first = source(&fixture.runtime, 1, "123", None, "same-label").await;
        let mut view = new_view(&first);
        for request in 1..=12 {
            command_call(&mut view, &fixture.runtime, request).await;
        }
        let id = group_id(&view, &first);
        view.tool_summary_set(&first, &id, "Keep this group folded", Some(0))
            .unwrap();
        view.select(isize::MIN);
        assert!(view.back());
        let first_canvas = view.position();
        assert_eq!(first_canvas.tool_groups.len(), 1);
        let saved_ids = view.events.iter().map(|event| event.id).collect::<Vec<_>>();
        let ui = Ui {
            color: false,
            ..Ui::default()
        };

        // A second canvas has its own default, while the first stays inactive.
        view.restore_position(&super::super::EventPosition::default());
        view.render(120, 24, &ui, false);
        assert!(view.tool_groups()[0].expanded);
        let second_canvas = view.position();
        for request in 13..=(MAX_EVENTS as u64 + 12) {
            command_call(&mut view, &fixture.runtime, request).await;
        }
        assert_eq!(view.events.len(), MAX_EVENTS);
        assert!(
            view.events
                .iter()
                .all(|event| !saved_ids.contains(&event.id))
        );
        assert_eq!(view.tool_groups().len(), 1);
        assert_eq!(group_id(&view, &first), id);
        assert_eq!(
            view.snapshot()["tool_groups"][0]["call_count"],
            MAX_EVENTS as u64 + 12
        );
        assert!(view.tool_groups()[0].expanded);
        assert!(view.position().tool_groups.len() <= MAX_EVENTS);

        for width in [120, 48] {
            view.restore_position(&first_canvas);
            let text = view.render(width, 24, &ui, true).join("\n");
            let group = view.tool_groups()[0].clone();
            assert!(!group.expanded);
            assert!(!view.events[group.start].group_automatic);
            assert_eq!(view.selected, Some(group.head));
            assert_eq!(view.selected_group, Some(group.head));
            assert_eq!(text.matches("Keep this group folded").count(), 1, "{text}");
            assert!(!text.contains("printf"), "{text}");

            view.restore_position(&second_canvas);
            let text = view.render(width, 24, &ui, false).join("\n");
            assert!(view.tool_groups()[0].expanded);
            assert!(view.events[view.tool_groups()[0].start].group_automatic);
            assert!(text.contains("Ran printf command-"), "{text}");
        }
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
        view.summary_call_started(&first, "ssmcp.fs.stat", false);
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
        view.summary_call_started(&bound, "ssmcp.fs.stat", false);
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
                    diff_mode: super::super::DiffMode::Single,
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
async fn group_identity_and_authorization_survive_eviction() {
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
        assert_eq!(
            view.snapshot()["tool_groups"][0]["summary"],
            "Inspecting many files"
        );
        assert_eq!(view.snapshot()["tool_groups"][0]["summary_editable"], true);
        view.tool_summary_set(&first, &id, "Live summary remains editable", Some(1))
            .unwrap();
    })
    .await;
}

#[tokio::test]
async fn native_read_paths_and_scoped_failures_remain_in_one_logical_group() {
    let fixture = Fixture::new().await;
    history::with_test_history(fixture.root.join("history"), async {
        let first = source(&fixture.runtime, 1, "123", None, "same-label").await;
        let mut view = new_view(&first);
        view.summary_call_started(&first, "ssmcp.fs.read_text", false);
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
            view.summary_call_started(&failed, "ssmcp.fs.stat", false);
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
            view.summary_call_started(&call, name, false);
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

#[tokio::test]
async fn command_summary_scroll_and_explicit_fold_survive_canvas_position_changes() {
    let fixture = Fixture::new().await;
    history::with_test_history(fixture.root.join("summary-position-history"), async {
        let first = source(&fixture.runtime, 1, "123", None, "same-label").await;
        let mut view = new_view(&first);
        command_call(&mut view, &fixture.runtime, 1).await;
        for request in 2..=18 {
            stat(&mut view, &fixture.runtime, request).await;
        }
        let id = group_id(&view, &first);
        view.tool_summary_set(&first, &id, "Inspect command results", Some(0))
            .unwrap();
        let ui = Ui {
            color: false,
            ..Ui::default()
        };
        view.render(120, 24, &ui, true);
        let group = view.tool_groups()[0].clone();
        view.scroll_tool_group(group.head, isize::MIN + 1);
        assert!(view.scroll_tool_group(group.head, 2));
        view.render(120, 24, &ui, true);
        let inner_scroll = view.events[group.start].group_panel.scroll;
        let outer_scroll = view.scroll;
        assert_eq!(view.events[group.start].group_panel.offset(), 2);
        view.tool_summary_set(&first, &id, "Checked command results", Some(1))
            .unwrap();
        view.render(48, 24, &ui, true);
        assert_eq!(view.events[group.start].group_panel.scroll, inner_scroll);
        assert_eq!(view.scroll, outer_scroll);
        let group = view.tool_groups()[0].clone();
        assert!(view.set_tool_expanded(&group, false));
        let saved_position = view.position();
        view.restore_position(&super::super::EventPosition::default());
        view.restore_position(&saved_position);
        for width in [48, 120] {
            let text = view.render(width, 24, &ui, true).join("\n");
            assert_eq!(text.matches("Checked command results").count(), 1, "{text}");
            assert!(!view.tool_groups()[0].expanded, "{text}");
        }
        let group = view.tool_groups()[0].clone();
        assert_eq!(view.events[group.start].group_panel.scroll, inner_scroll);
        assert!(!view.events[group.start].group_automatic);
        assert!(view.set_tool_expanded(&group, true));
        view.render(120, 24, &ui, true);
        assert_eq!(view.events[group.start].group_panel.offset(), 2);
        assert_eq!(view.snapshot()["tool_groups"][0]["summary_editable"], true);
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

#[tokio::test]
async fn coordinator_polling_interleaves_without_hiding_details_or_new_input() {
    let fixture = Fixture::new().await;
    history::with_test_history(fixture.root.join("history"), async {
        let owner = source(&fixture.runtime, 1, "123", None, "operator").await;
        let mut view = new_view(&owner);
        let input = |view: &mut EventView, origin: &str, text: &str| {
            view.ingest_scoped_event(
                "message",
                &json!({"type":"ssmcp.message","role":"tool","format":"plain",
                    "kind":"input","input_origin":origin,"text":text}),
                b"",
                owner.display(),
            );
        };
        let gap = |subscription: &str, cursor: u64, retained: u64| {
            let data = json!({"status":"unavailable","subscription_id":subscription,
                "error":format!("events_gap: cursor {cursor} precedes retained cursor {retained}; latest cursor 71289; resubscribe with an explicit retained cursor after reconciling the gap")});
            format!("WORKER STATE EVENTS — retained until explicit events.ack\n{data}")
        };
        let policy = "Continue authorized work; otherwise await these events (30000 ms, same wait_id). ACK processed cursors. ui.wait delivers input.";
        let standby = "Worker pool pool-a: 3 connected external worker(s) are standing by. Use ssmcp.worker.submit for independent work.";
        for i in 0..3 {
            let call = source(&fixture.runtime, 10 + i, "123", None, "operator").await;
            view.summary_call_started(&call, "ssmcp.time.prompt", true);
            view.summary_call_finished(&call, true);
            input(&mut view, "worker_event", &gap("sub-a", 70243, 70265 + i));
            input(&mut view, "system_hook", standby);
            input(&mut view, "system_hook", policy);
        }
        assert_eq!(view.visible_ids(), vec![0, 1, 2, 3]);
        let ui = Ui {
            color: false,
            ..Ui::default()
        };
        let rows = view.render(120, 22, &ui, true).join("\n");
        for expected in [
            "ssmcp.time.prompt · done ×3",
            "恢复订阅 ×3",
            "观察时 3 个工位空闲 ×3",
            "协调者等待策略 ×3",
        ] {
            assert!(rows.contains(expected), "{rows}");
        }
        view.select(isize::MIN);
        view.select(1);
        assert!(view.open());
        let expanded = view.render(230, 40, &ui, true).join("\n");
        for retained in 70265..=70267 {
            assert!(
                expanded.contains(&format!("precedes retained cursor {retained}")),
                "{expanded}"
            );
        }
        assert!(view.back());
        view.filter("retained cursor 70266", 0);
        assert_eq!(view.visible_ids(), vec![5]);
        view.filter("", 0);
        view.set_scope(EventScope::All);
        assert_eq!(view.visible_ids(), (0..12).collect::<Vec<_>>());
        view.set_scope(EventScope::Conversation);
        let user = gap("sub-a", 70243, 70268);
        view.ingest_scoped_event(
            "message",
            &json!({"type":"ssmcp.message","role":"user","text":user}),
            b"",
            owner.display(),
        );
        input(&mut view, "worker_event", &user);
        input(&mut view, "worker_event", &gap("sub-a", 70244, 70270));
        input(&mut view, "worker_event", &gap("sub-b", 70244, 70270));
        input(
            &mut view,
            "system_hook",
            &format!("{policy} Maintenance round-1: one authorized action."),
        );
        let unknown =
            "WORKER STATE EVENTS — retained until explicit events.ack\n{\"kind\":\"worker.unknown\"}";
        input(&mut view, "worker_event", unknown);
        for (id, success, read) in [(30, false, true), (31, true, false)] {
            let call = source(&fixture.runtime, id, "123", None, "operator").await;
            view.summary_call_started(&call, "ssmcp.time.prompt", read);
            view.summary_call_finished(&call, success);
            input(&mut view, "worker_event", &gap("sub-b", 70244, 70271));
        }
        assert_eq!(
            view.visible_ids(),
            [vec![0, 1, 2, 3], (12..22).collect()].concat()
        );
        let rows = view.render(200, 100, &ui, true).join("\n");
        assert!(
            rows.contains("precedes retained cursor 70268")
                && rows.contains("维护请求")
                && rows.contains("任务结果未知")
                && rows.contains("ssmcp.time.prompt · failed"),
            "{rows}"
        );
    })
    .await;
}

#[tokio::test]
async fn repeating_notices_do_not_cross_controller_or_connection_boundaries() {
    let fixture = Fixture::new().await;
    history::with_test_history(fixture.root.join("history"), async {
        let owner = source(&fixture.runtime, 1, "123", None, "same-label").await;
        let other = source(&fixture.runtime, 2, "456", None, "same-label").await;
        let second_fixture = Fixture::new().await;
        let other_connection = source(&second_fixture.runtime, 3, "123", None, "same-label").await;
        let mut view = new_view(&owner);
        let body = json!({"type":"ssmcp.message","role":"tool","kind":"input","input_origin":"worker_event",
            "text":"WORKER STATE EVENTS — retained until explicit events.ack\n{\"status\":\"unavailable\",\"subscription_id\":\"one\",\"error\":\"events_gap: cursor 1 precedes retained cursor 2\"}"});
        for controller in [&owner, &other, &owner, &other_connection, &owner] {
            let mut display = controller.display();
            // Exercise the discriminator even when one local pane selects all five records.
            display.history = owner.display().history;
            view.ingest_scoped_event("message", &body, b"", display);
        }
        assert_eq!(view.visible_ids(), vec![0, 1, 2, 3, 4]);
    })
    .await;
}
