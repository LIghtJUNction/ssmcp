use super::*;
use crate::{
    backend::shell::Shell,
    output::OutputStore,
    runtime::call_scope::CallScope,
    wire::{CodeContext, ControlAccount, ControlAgent, ControlAttribution},
};

const ISOLATED: &str = "SSMCP_WORKER_HOOK_ISOLATED_TEST";
const FIXTURE: &str = "SSMCP_WORKER_HOOK_COMMAND_FIXTURE";
const TEST_NAME: &str = "worker::pool::command_logs_tests::native_exec_records_real_start_and_exit_on_its_original_pool";
const FIXTURE_NAME: &str = "worker::pool::command_logs_tests::native_command_fixture";

fn source(client: &str, participant: &str, connection: u64) -> Source {
    let attribution = ControlAttribution {
        account: ControlAccount {
            provider: "github".into(),
            subject: "123".into(),
            login: "fixture".into(),
        },
        owner: None,
        agent: Some(ControlAgent {
            id: participant.into(),
            name: "worker fixture".into(),
        }),
    };
    let context = CodeContext {
        run_id: "c".repeat(32),
        code: "fixture".into(),
        timestamp_ms: 1,
        session_id: Some(client.into()),
        thread_id: Some(participant.into()),
    };
    let mut source = Source::default();
    source.call = Some(
        CallScope::from_relay_for_client(
            connection,
            Some(client),
            1,
            Some(&attribution),
            Some(&context),
        )
        .unwrap(),
    );
    source
}

// This real command waits for EOF so the parent can change the current display
// pool before it exits. It launches no model and relies on no external shell.
#[test]
fn native_command_fixture() {
    if std::env::var(FIXTURE).as_deref() != Ok("1") {
        return;
    }
    use std::io::{Read, Write};
    let mut input = Vec::new();
    std::io::stdin().read_to_end(&mut input).unwrap();
    println!("worker native hook fixture");
    std::io::stdout().flush().unwrap();
    std::process::exit(7);
}

#[tokio::test]
async fn native_exec_records_real_start_and_exit_on_its_original_pool() {
    // Pool display is process-global in production. Run this end-to-end case
    // alone so unrelated parallel unit tests cannot replace its display pool.
    if std::env::var(ISOLATED).as_deref() != Ok("1") {
        let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", TEST_NAME, "--nocapture"])
            .env(ISOLATED, "1")
            .kill_on_drop(true);
        let result = tokio::time::timeout(Duration::from_secs(30), command.output())
            .await
            .expect("isolated worker command hook test timed out")
            .unwrap();
        assert!(
            result.status.success(),
            "isolated hook test failed: {}\n{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        return;
    }

    let client = "d".repeat(64);
    let worker_id = format!("agent_{}", "b".repeat(32));
    let owner_id = format!("agent_{}", "a".repeat(32));
    let worker_source = source(&client, &worker_id, 71);
    let owner_source = source(&client, &owner_id, 71);
    let worker = actor(&worker_source).unwrap();
    let owner = actor(&owner_source).unwrap();
    let pool = Pool::new(
        Config {
            count: 1,
            factor: 1.0,
        },
        &Agent::Remote,
        PathBuf::new(),
    )
    .unwrap();
    pool.start();
    let task = pool
        .queue
        .submit_managed(
            owner.clone(),
            &client,
            "native-command-task",
            "Inspect native command evidence",
            TaskContext {
                #[cfg(target_os = "linux")]
                source: owner_source,
                #[cfg(target_os = "linux")]
                actor_key: actor_context_key(&owner).unwrap(),
                prepared: "Inspect native command evidence".into(),
            },
            1,
        )
        .unwrap();
    let Claimed::Assigned { claim, .. } = pool
        .remote()
        .unwrap()
        .claim(&pool.queue, &worker, "take-task", 2)
        .unwrap()
    else {
        panic!("worker did not receive the real task claim")
    };

    let command_argv = vec![
        std::env::current_exe()
            .unwrap()
            .to_string_lossy()
            .into_owned(),
        "--exact".into(),
        FIXTURE_NAME.into(),
        "--nocapture".into(),
    ];
    let other_client = source(&"e".repeat(64), &worker_id, 71);
    let other_connection = source(&client, &worker_id, 72);
    let other_agent = source(&client, &format!("agent_{}", "f".repeat(32)), 71);
    for rejected in [
        &other_client,
        &other_connection,
        &other_agent,
        &Source::default(),
    ] {
        assert!(
            crate::worker::native_command_started(rejected, "must-not-log", &command_argv)
                .is_none()
        );
    }
    let shell = Shell::with_workspace(
        std::env::current_dir().unwrap(),
        OutputStore::new_connection().unwrap(),
    )
    .unwrap();
    let started = shell
        .call_scoped(
            "ssmcp.exec.start",
            json!({
                "command":command_argv,"stdin":"pipe","timeout_ms":10000,
                "env":{(FIXTURE):"1"}
            }),
            worker_source.clone(),
        )
        .await
        .unwrap();
    let job_id = started["job_id"].as_str().unwrap().to_owned();
    assert_eq!(started["running"], true);
    let initial = pool.queue.snapshot();
    let starts: Vec<_> = initial.slots[0]
        .logs
        .iter()
        .filter(|event| event.text.starts_with("Command started:"))
        .collect();
    assert_eq!(starts.len(), 1);
    assert_eq!(starts[0].task_id.as_deref(), Some(task.task_id.as_str()));
    assert!(starts[0].text.contains(FIXTURE_NAME));

    // A later pool and an unrelated status reader cannot retag this running job.
    let replacement = Pool::new(
        Config {
            count: 1,
            factor: 1.0,
        },
        &Agent::Remote,
        PathBuf::new(),
    )
    .unwrap();
    replacement.start();
    for _ in 0..2 {
        shell
            .call_scoped(
                "ssmcp.exec.read",
                json!({"job_id":job_id,"wait_ms":0}),
                other_agent.clone(),
            )
            .await
            .unwrap();
    }
    assert_eq!(
        pool.queue.snapshot().slots[0]
            .logs
            .iter()
            .filter(|event| event.text.starts_with("Command started:"))
            .count(),
        1
    );
    assert!(
        !pool.queue.snapshot().slots[0]
            .logs
            .iter()
            .any(|event| event.text.starts_with("Command finished:"))
    );
    shell
        .call_scoped(
            "ssmcp.exec.write",
            json!({"job_id":job_id,"close_stdin":true}),
            worker_source,
        )
        .await
        .unwrap();
    let done = shell
        .call("ssmcp.exec.wait", json!({"job_id":job_id,"wait_ms":5000}))
        .await
        .unwrap();
    assert_eq!(done["running"], false);
    assert_eq!(done["exit_code"], 7);

    // A completion status may be read just before the finishing task appends
    // its log; wait for the actual queue event, never manufacture an end event.
    let mut changed = pool.queue.subscribe();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if pool.queue.snapshot().slots[0].logs.iter().any(|event| {
                event.text.starts_with("Command finished:") && event.text.contains("exit 7")
            }) {
                break;
            }
            changed.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    let view = pool.queue.snapshot();
    let ends: Vec<_> = view.slots[0]
        .logs
        .iter()
        .filter(|event| event.text.starts_with("Command finished:"))
        .collect();
    assert_eq!(ends.len(), 1);
    assert_eq!(ends[0].task_id.as_deref(), Some(task.task_id.as_str()));
    assert!(
        replacement.queue.snapshot().slots[0]
            .logs
            .iter()
            .all(|event| !event.text.starts_with("Command "))
    );
    assert_eq!(
        view.working, 1,
        "native command completion does not complete its parent task"
    );
    pool.remote()
        .unwrap()
        .complete(
            &pool.queue,
            &worker,
            Completion {
                claim,
                status: remote::Status::Completed,
                output: Some("observed real exit 7".into()),
                error: None,
            },
            3,
        )
        .unwrap();
    shell.shutdown().await;
    pool.shutdown().await.unwrap();
    replacement.shutdown().await.unwrap();
}
