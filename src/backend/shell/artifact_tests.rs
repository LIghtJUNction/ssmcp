use super::*;
use crate::{
    runtime::call_scope::{CallScope, Source},
    wire::{CodeContext, ControlAccount, ControlAgent, ControlAttribution},
};
use std::{fs, os::unix::fs::symlink};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "ssmcp-artifacts-test-{}-{}",
            std::process::id(),
            NEXT_JOB.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn source(agent: char, subject: &str, connection: u64, request: u64) -> Source {
    let client = "fixture-client";
    let agent_id = format!("agent_{}", agent.to_string().repeat(32));
    let mut source = Source::default();
    source.call = Some(
        CallScope::from_relay_for_client(
            connection,
            Some(client),
            request,
            Some(&ControlAttribution {
                account: ControlAccount {
                    provider: "github".into(),
                    subject: subject.into(),
                    login: "test".into(),
                },
                owner: None,
                agent: Some(ControlAgent {
                    id: agent_id.clone(),
                    name: "same label".into(),
                }),
            }),
            Some(&CodeContext {
                run_id: format!("{request:032x}"),
                code: "return 1".into(),
                timestamp_ms: 1,
                session_id: Some(client.into()),
                thread_id: Some(agent_id),
            }),
        )
        .unwrap(),
    );
    source
}

#[tokio::test]
async fn tracked_exec_counts_child_and_workspace_writes_and_isolates_notices() {
    let fixture = Fixture::new();
    let root = fixture.0.join("workspace");
    fs::create_dir(&root).unwrap();
    let output = OutputStore::new_connection().unwrap();
    let shell = Shell::with_workspace(root.clone(), output.clone()).unwrap();
    let workspace = crate::workspace::Workspace::with_output(root.clone(), output).unwrap();
    let command = json!([
        "/bin/sh",
        "-c",
        r#"test "$TMPDIR" = "$SSMCP_ARTIFACT_DIR" && mkdir "$TMPDIR/build" && printf 123456 > "$TMPDIR/build/child""#
    ]);
    let initiator = source('a', "42", 81, 1);
    let later_call = source('a', "42", 81, 2);
    let other_agent = source('b', "42", 81, 3);

    for budget in [0, 10] {
        assert!(
            shell
                .call_scoped(
                    "ssmcp.exec.start",
                    json!({"command":command,"artifact_budget_bytes":budget}),
                    Source::default()
                )
                .await
                .is_err()
        );
    }
    assert_eq!(fs::read_dir(&root).unwrap().count(), 0);

    let start = shell.call_scoped(
        "ssmcp.exec.start",
        json!({"command":command,"artifact_budget_bytes":10,
            "env":{"TMPDIR":"/not-the-job-directory","SSMCP_ARTIFACT_DIR":"/not-the-job-directory"}}),
        initiator
    ).await.unwrap();
    let id = start["job_id"].as_str().unwrap();
    let directory = start["artifacts"]["directory"].as_str().unwrap();
    let task = root.join(directory);
    let status = |caller| shell.call_scoped("ssmcp.exec.status", json!({"job_id":id}), caller);

    let done = shell
        .call_scoped(
            "ssmcp.exec.wait",
            json!({"job_id":id,"wait_ms":5000}),
            later_call.clone(),
        )
        .await
        .unwrap();
    assert_eq!(done["exit_code"], 0);
    let child = status(later_call.clone()).await.unwrap();
    assert_eq!(child["artifacts"]["logical_bytes"], 6);
    assert_eq!(child["artifacts"]["incomplete"], false);

    let second = shell
        .call_scoped(
            "ssmcp.exec.start",
            json!({"command":["/bin/true"],"artifact_budget_bytes":10}),
            other_agent.clone(),
        )
        .await
        .unwrap();
    let second_id = second["job_id"].as_str().unwrap();
    let second_dir = second["artifacts"]["directory"].as_str().unwrap();
    assert_ne!(directory, second_dir);
    fs::write(root.join(second_dir).join("other"), "abcdefghijkl").unwrap();

    let external = fixture.0.join("outside");
    fs::create_dir(&external).unwrap();
    fs::write(external.join("data"), "external-content").unwrap();
    symlink(&external, task.join("escape")).unwrap();
    symlink(external.join("data"), task.join("escape-file")).unwrap();
    fs::hard_link(external.join("data"), task.join("shared-inode")).unwrap();
    fs::write(root.join("shared"), "unattributed").unwrap();
    assert!(
        workspace
            .perform(
                "ssmcp.fs.write_text",
                json!({"path":format!("{directory}/escape/data"),"text":"x","overwrite":true})
            )
            .await
            .is_err()
    );
    workspace
        .perform(
            "ssmcp.fs.write_text",
            json!({"path":format!("{directory}/harness"),"text":"123456"}),
        )
        .await
        .unwrap();

    for unauthorized in [
        other_agent.clone(),
        source('a', "43", 81, 4),
        source('a', "42", 82, 5),
        Source::default(),
    ] {
        assert!(
            status(unauthorized)
                .await
                .unwrap()
                .get("artifacts")
                .is_none()
        );
    }
    assert!(
        shell
            .call_scoped(
                "ssmcp.exec.status",
                json!({"job_id":second_id}),
                later_call.clone()
            )
            .await
            .unwrap()
            .get("artifacts")
            .is_none()
    );

    let first = shell
        .call_scoped("ssmcp.exec.read", json!({"job_id":id}), later_call.clone())
        .await
        .unwrap();
    let first = &first["snapshot"]["artifacts"];
    assert_eq!(first["logical_bytes"], 12);
    assert_eq!(first["notice"]["logical_bytes"], 12);
    assert_eq!(first["notice"]["soft_limit_bytes"], 10);
    assert_eq!(
        first["notice"]["largest_files"][0]["relative_path"],
        "build/child"
    );
    assert_eq!(
        first["notice"]["largest_files"][1]["relative_path"],
        "harness"
    );

    fs::rename(task.join("harness"), task.join("new-name")).unwrap();
    fs::rename(task.join("build"), task.join("a-earlier-directory")).unwrap();
    let current = status(later_call.clone()).await.unwrap();
    assert_eq!(current["artifacts"]["logical_bytes"], 12);
    assert_eq!(current["artifacts"]["incomplete"], false);
    assert!(current["artifacts"].get("notice").is_none());
    fs::write(task.join("a-earlier-directory/child"), "abcdefgh").unwrap();
    fs::remove_file(task.join("new-name")).unwrap();
    symlink(external.join("data"), task.join("new-name")).unwrap();
    let low = status(later_call.clone()).await.unwrap();
    assert_eq!(low["artifacts"]["logical_bytes"], 8);
    assert!(low["artifacts"].get("notice").is_none());

    fs::write(task.join("again"), "1234").unwrap();
    fs::write(task.join("again"), "12345").unwrap();
    let again = status(later_call.clone()).await.unwrap();
    assert_eq!(again["artifacts"]["logical_bytes"], 13);
    assert_eq!(
        again["artifacts"]["notice"]["largest_files"][0]["relative_path"],
        "a-earlier-directory/child"
    );
    let repeat = status(later_call.clone()).await.unwrap();
    assert!(repeat["artifacts"].get("notice").is_none());

    let second_status = shell
        .call_scoped(
            "ssmcp.exec.status",
            json!({"job_id":second_id}),
            other_agent,
        )
        .await
        .unwrap();
    assert_eq!(second_status["artifacts"]["notice"]["logical_bytes"], 12);
    let listed = shell.call("ssmcp.exec.list", json!({})).await.unwrap();
    assert!(
        listed["jobs"]
            .as_array()
            .unwrap()
            .iter()
            .all(|job| job.get("artifacts").is_none())
    );

    fs::rename(&task, external.join("moved-root")).unwrap();
    symlink(external.join("moved-root"), &task).unwrap();
    let moved = status(later_call).await.unwrap();
    assert_eq!(moved["artifacts"]["incomplete"], true);
    assert!(moved["artifacts"]["logical_bytes"].is_null());
    assert_eq!(
        fs::read(external.join("data")).unwrap(),
        b"external-content"
    );
    shell.shutdown().await;
}

#[test]
fn event_pressure_marks_incomplete_and_reconciles_the_dedicated_root() {
    let fixture = Fixture::new();
    let mut tracker = artifacts::Artifacts::new(&fixture.0, "queue-test", 10).unwrap();
    fs::write(tracker.path.join("known"), "123456").unwrap();
    let outside = fixture.0.join("shared");
    fs::write(&outside, "not in the dedicated root").unwrap();
    symlink(&outside, tracker.path.join("link")).unwrap();

    // More distinct events than either the local batch cap or the kernel queue
    // can retain. Only the small final tree is eligible for reconciliation.
    for i in 0..12_000 {
        let path = tracker.path.join(format!("transient-event-{i:05}"));
        fs::write(&path, "x").unwrap();
        fs::remove_file(path).unwrap();
    }
    let recovered = tracker.sample();
    assert_eq!(recovered["incomplete"], true);
    assert_eq!(recovered["logical_bytes"], 6);
    fs::write(tracker.path.join("known"), "12345678").unwrap();
    let next = tracker.sample();
    assert_eq!(next["incomplete"], true);
    assert_eq!(next["logical_bytes"], 8);
}
