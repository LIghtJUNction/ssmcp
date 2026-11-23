use super::*;
use crate::{
    runtime::call_scope::{CallScope, Source},
    wire::{CodeContext, ControlAccount, ControlAgent, ControlAttribution},
};
use std::{fs, os::unix::fs::symlink};

#[tokio::test]
async fn tracked_exec_failed_spawn_does_not_leave_unreported_directories() {
    let fixture = Fixture::new();
    let shell =
        Shell::with_workspace(fixture.0.clone(), OutputStore::new_connection().unwrap()).unwrap();
    let owner = source('a', "42", 81, 1);
    let missing = fixture.0.join("missing-executable");
    for mut arguments in [
        json!({"track_artifacts":true}),
        json!({"artifact_budget_bytes":10}),
        json!({"temporary_artifacts":true}),
    ] {
        arguments["command"] = json!([missing]);
        let error = shell
            .call_scoped("ssmcp.exec.start", arguments, owner.clone())
            .await
            .unwrap_err();
        assert!(error.contains("could not start command"), "{error}");
        assert_eq!(
            fs::read_dir(fixture.0.join(".ssmcp/task-artifacts"))
                .unwrap()
                .count(),
            0
        );
    }
    assert!(
        shell.call("ssmcp.exec.list", json!({})).await.unwrap()["jobs"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    let job = shell
        .call_scoped(
            "ssmcp.exec.start",
            json!({"command":["/bin/true"],"track_artifacts":true}),
            owner,
        )
        .await
        .unwrap();
    let directory = fixture
        .0
        .join(job["artifacts"]["directory"].as_str().unwrap());
    let done = shell
        .call("ssmcp.exec.wait", json!({"job_id":job["job_id"]}))
        .await
        .unwrap();
    assert_eq!(done["exit_code"], 0);
    shell
        .call("ssmcp.exec.forget", json!({"job_id":job["job_id"]}))
        .await
        .unwrap();
    shell.shutdown().await;
    assert!(
        directory.is_dir(),
        "successful jobs retain their directories"
    );
}

#[test]
fn unstarted_artifacts_remove_only_their_own_empty_directory() {
    let fixture = Fixture::new();
    let tracker = artifacts::Artifacts::new(&fixture.0, 10).unwrap();
    let path = tracker.path.clone();
    drop(tracker);
    assert!(!path.exists());

    let tracker = artifacts::Artifacts::new(&fixture.0, 10).unwrap();
    let kept = tracker.path.join("keep");
    fs::write(&kept, b"owned data").unwrap();
    drop(tracker);
    assert_eq!(fs::read(&kept).unwrap(), b"owned data");
}

#[test]
fn unstarted_artifacts_preserve_replaced_directory_and_symlink() {
    let fixture = Fixture::new();
    for use_link in [false, true] {
        let tracker = artifacts::Artifacts::new(&fixture.0, 10).unwrap();
        let path = tracker.path.clone();
        let moved = path.with_extension("moved");
        fs::rename(&path, &moved).unwrap();
        if use_link {
            symlink(&moved, &path).unwrap();
        } else {
            fs::create_dir(&path).unwrap();
        }
        drop(tracker);
        assert!(moved.is_dir());
        assert!(fs::symlink_metadata(&path).is_ok());
    }
}

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
            json!({"command":["/bin/true"],"track_artifacts":true,"artifact_budget_bytes":10}),
            other_agent.clone(),
        )
        .await
        .unwrap();
    let second_id = second["job_id"].as_str().unwrap();
    let second_dir = second["artifacts"]["directory"].as_str().unwrap();
    assert_ne!(directory, second_dir);
    assert_eq!(second["artifacts"]["soft_limit_bytes"], 10);
    for path in [directory, second_dir] {
        let token = path.strip_prefix(".ssmcp/task-artifacts/").unwrap();
        assert_eq!(token.len(), 16);
        assert!(token.bytes().all(|byte| byte.is_ascii_hexdigit()));
    }
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
    let mut tracker = artifacts::Artifacts::new(&fixture.0, 10).unwrap();
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

#[tokio::test]
async fn tracked_exec_default_budget_is_opt_in_and_rearms_at_low_water() {
    let fixture = Fixture::new();
    let outside = fixture.0.join("outside");
    let workspace = fixture.0.join("workspace");
    fs::create_dir(&outside).unwrap();
    fs::create_dir(&workspace).unwrap();
    let shell =
        Shell::with_workspace(workspace.clone(), OutputStore::new_connection().unwrap()).unwrap();
    let owner = source('a', "42", 81, 1);
    let ordinary = shell.call_scoped("ssmcp.exec.start", json!({
        "command": ["/bin/sh", "-c", "test \"$TMPDIR\" = caller-tmp && test \"$SSMCP_ARTIFACT_DIR\" = caller-dir"],
        "env": {"TMPDIR":"caller-tmp","SSMCP_ARTIFACT_DIR":"caller-dir"}
    }), owner.clone()).await.unwrap();
    let done = shell
        .call("ssmcp.exec.wait", json!({"job_id":ordinary["job_id"]}))
        .await
        .unwrap();
    assert_eq!(done["exit_code"], 0);
    assert!(ordinary.get("artifacts").is_none());
    assert!(!workspace.join(".ssmcp").exists());

    fs::create_dir(workspace.join(".ssmcp")).unwrap();
    let managed = workspace.join(".ssmcp/task-artifacts");
    symlink(&outside, &managed).unwrap();
    let args = json!({"command":["/bin/true"],"track_artifacts":true});
    assert!(
        shell
            .call_scoped("ssmcp.exec.start", args.clone(), owner.clone())
            .await
            .is_err()
    );
    assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
    fs::remove_file(&managed).unwrap();

    let job = shell
        .call_scoped("ssmcp.exec.start", args, owner.clone())
        .await
        .unwrap();
    let budget = DEFAULT_ARTIFACT_BUDGET_BYTES;
    assert_eq!(budget, 20_000_000_000);
    assert_eq!(job["artifacts"]["soft_limit_bytes"], budget);
    let file = fs::File::create(
        workspace
            .join(job["artifacts"]["directory"].as_str().unwrap())
            .join("sparse"),
    )
    .unwrap();
    let status = || {
        shell.call_scoped(
            "ssmcp.exec.status",
            json!({"job_id":job["job_id"]}),
            owner.clone(),
        )
    };
    file.set_len(budget).unwrap();
    assert!(status().await.unwrap()["artifacts"].get("notice").is_none());
    file.set_len(budget + 1).unwrap();
    assert_eq!(
        status().await.unwrap()["artifacts"]["notice"]["logical_bytes"],
        budget + 1
    );
    file.set_len(budget * 4 / 5 + 1).unwrap();
    assert!(status().await.unwrap()["artifacts"].get("notice").is_none());
    file.set_len(budget + 2).unwrap();
    assert!(status().await.unwrap()["artifacts"].get("notice").is_none());
    file.set_len(budget * 4 / 5).unwrap();
    assert!(status().await.unwrap()["artifacts"].get("notice").is_none());
    file.set_len(budget + 3).unwrap();
    assert_eq!(
        status().await.unwrap()["artifacts"]["notice"]["logical_bytes"],
        budget + 3
    );
    shell.shutdown().await;
}

#[tokio::test]
async fn shutdown_reclaims_only_explicitly_temporary_unreferenced_jobs() {
    let fixture = Fixture::new();
    let shell =
        Shell::with_workspace(fixture.0.clone(), OutputStore::new_connection().unwrap()).unwrap();
    let second =
        Shell::with_workspace(fixture.0.clone(), OutputStore::new_connection().unwrap()).unwrap();
    let owner = source('a', "42", 81, 1);

    async fn start(shell: &Shell, owner: &Source, temporary: bool, script: &str) -> Value {
        shell
            .call_scoped(
                "ssmcp.exec.start",
                json!({
                    "command":["/bin/sh","-c",script],
                    "track_artifacts":!temporary, "temporary_artifacts":temporary
                }),
                owner.clone(),
            )
            .await
            .unwrap()
    }
    async fn wait(shell: &Shell, job: &Value) {
        let done = shell
            .call("ssmcp.exec.wait", json!({"job_id":job["job_id"]}))
            .await
            .unwrap();
        assert_eq!(done["exit_code"], 0);
    }
    let path = |job: &Value| {
        fixture
            .0
            .join(job["artifacts"]["directory"].as_str().unwrap())
    };
    let write = r#"mkdir "$TMPDIR/sub" && printf bytes > "$TMPDIR/sub/payload""#;
    let reclaimed = start(&shell, &owner, true, write).await;
    let forgotten = start(&shell, &owner, true, write).await;
    let persisted = start(&shell, &owner, false, write).await;
    let held = start(&shell, &owner, true, write).await;
    let open_file = start(&shell, &owner, true, write).await;
    let foreign = start(&second, &owner, true, write).await;
    for job in [&reclaimed, &forgotten, &persisted, &held, &open_file] {
        wait(&shell, job).await;
    }
    wait(&second, &foreign).await;
    shell
        .call("ssmcp.exec.forget", json!({"job_id":forgotten["job_id"]}))
        .await
        .unwrap();
    let reference = shell.job(held["job_id"].as_str().unwrap()).unwrap();
    let file_reference = fs::File::open(path(&open_file).join("sub/payload")).unwrap();

    let parent = path(&reclaimed).parent().unwrap().to_owned();
    let legacy = parent.join("unclaimed");
    fs::create_dir(&legacy).unwrap();
    fs::write(legacy.join("keep"), "old").unwrap();
    let config = fixture.0.join(".ssmcp/config.toml");
    fs::write(&config, "keep configuration").unwrap();

    let active = start(
        &shell,
        &owner,
        true,
        r#"printf live > "$TMPDIR/live" && printf ready && exec /bin/sleep 60"#,
    )
    .await;
    // A file can exist before the child has written it. The child emits
    // this marker only after the write and the redirection have completed.
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut stdout = String::new();
        let mut cursor = 0;
        while stdout.len() < "ready".len() {
            let page = shell
                .call(
                    "ssmcp.exec.read",
                    json!({"job_id":active["job_id"], "stdout_cursor":cursor, "wait_ms":1000}),
                )
                .await
                .unwrap();
            cursor = page["stdout_cursor"].as_u64().unwrap();
            stdout.push_str(page["stdout"].as_str().unwrap());
            assert_eq!(page["running"], true, "{page}");
        }
        assert_eq!(stdout, "ready");
    })
    .await
    .expect("the active fixture never confirmed its completed write");

    // Client close and reload both invoke this same shutdown path.
    shell.shutdown().await;
    assert!(!path(&reclaimed).exists());
    assert!(!path(&forgotten).exists());
    for job in [&persisted, &held, &foreign, &open_file] {
        assert_eq!(fs::read(path(job).join("sub/payload")).unwrap(), b"bytes");
    }
    assert_eq!(fs::read(path(&active).join("live")).unwrap(), b"live");
    assert_eq!(fs::read(legacy.join("keep")).unwrap(), b"old");
    assert_eq!(fs::read(&config).unwrap(), b"keep configuration");
    drop((reference, file_reference));

    second.shutdown().await;
    assert!(!path(&foreign).exists());
}

#[tokio::test]
async fn shutdown_preserves_incomplete_or_escaped_temp_artifacts() {
    let fixture = Fixture::new();
    let shell =
        Shell::with_workspace(fixture.0.clone(), OutputStore::new_connection().unwrap()).unwrap();
    let owner = source('a', "42", 81, 1);
    let mut paths = Vec::new();
    for _ in 0..3 {
        let job = shell.call_scoped("ssmcp.exec.start", json!({
            "command":["/bin/sh","-c","printf safe > \"$TMPDIR/keep\""], "temporary_artifacts":true
        }), owner.clone()).await.unwrap();
        shell
            .call("ssmcp.exec.wait", json!({"job_id":job["job_id"]}))
            .await
            .unwrap();
        paths.push((
            job["job_id"].as_str().unwrap().to_owned(),
            fixture
                .0
                .join(job["artifacts"]["directory"].as_str().unwrap()),
        ));
    }
    let outside = fixture.0.join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("data"), "external").unwrap();
    symlink(&outside, paths[0].1.join("link")).unwrap();
    fs::rename(&paths[1].1, outside.join("moved")).unwrap();
    symlink(&outside, &paths[1].1).unwrap();
    for i in 0..9_000 {
        let path = paths[2].1.join(format!("transient-{i:05}"));
        fs::write(&path, "event").unwrap();
        fs::remove_file(path).unwrap();
    }
    assert_eq!(
        shell
            .call_scoped("ssmcp.exec.status", json!({"job_id":paths[2].0}), owner)
            .await
            .unwrap()["artifacts"]["incomplete"],
        true
    );
    shell.shutdown().await;
    assert_eq!(fs::read(paths[0].1.join("keep")).unwrap(), b"safe");
    assert_eq!(fs::read(outside.join("moved/keep")).unwrap(), b"safe");
    assert_eq!(fs::read(paths[2].1.join("keep")).unwrap(), b"safe");
    assert_eq!(fs::read(outside.join("data")).unwrap(), b"external");
}
