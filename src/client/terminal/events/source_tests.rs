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

struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn runtime() -> (Scratch, Arc<Backend>, Arc<Runtime>) {
    let root = Scratch(std::env::temp_dir().join(format!(
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
        assert_eq!(source.is_some(), expected.is_some());
        if let Some(expected) = expected {
            assert!(source.as_ref().unwrap().same_call(expected));
        }
        // Projection metadata is not injected into the original audit payload.
        assert!(event.get("source").is_none());
        assert!(event.get("attribution").is_none());
        let text = format!("[RUN] {id} {}\n", event["type"].as_str().unwrap());
        view.ingest_scoped_event("shell", &event, text.as_bytes(), source);
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
    view.ingest_scoped_event("shell", &native, b"[RUN] native\n", Some(alice.clone()));
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
    assert!(retained.unwrap().same_call(&alice));
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
