//! Native release smoke tests. No SSH server, coding harness or model is needed.
use serde_json::{Value, json};
use ssmcp::{backend::shell::Shell, context::Context, output::OutputStore, workspace::Workspace};
use std::{
    io::Write,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::time::timeout;

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "ssmcp-release-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path.canonicalize().unwrap())
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn write(workspace: &Workspace, path: &str, text: &str) {
    workspace
        .perform(
            "ssmcp.fs.write_text",
            json!({"path":path,"text":text,"create_parents":true}),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn native_files_skills_and_instruction_precedence_work_together() {
    let scratch = Scratch::new();
    let workspace = Workspace::new(scratch.0.clone()).unwrap();
    write(&workspace, "notes/中文.txt", "first\n界\n").await;
    let original = workspace
        .perform("ssmcp.fs.read_text", json!({"path":"notes/中文.txt"}))
        .await
        .unwrap();
    assert_eq!(original["text"], "first\n界\n");
    assert_eq!(original["eof"], true);
    let edited = workspace
        .perform(
            "ssmcp.fs.edit_text",
            json!({
                "path":"notes/中文.txt","old_text":"first","new_text":"changed",
                "expected_sha256":original["sha256"]
            }),
        )
        .await
        .unwrap();
    assert_eq!(edited["previous_sha256"], original["sha256"]);
    let audit = workspace
        .perform("ssmcp.fs.events", json!({}))
        .await
        .unwrap();
    let diff = audit["events"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .map(|entry| &entry["event"])
        .find(|event| event["type"] == "ssmcp.file.diff" && event["path"] == "notes/中文.txt")
        .expect("successful edit must publish its diff evidence");
    assert_eq!(diff["before"]["sha256"], original["sha256"]);
    assert_eq!(diff["after"]["sha256"], edited["sha256"]);
    assert_eq!(diff["added_lines"], 1);
    assert_eq!(diff["removed_lines"], 1);
    assert_eq!(diff["diff_complete"], true);
    let text = diff["diff"]
        .as_str()
        .expect("small complete diff is inline");
    assert!(text.contains("-first\n+changed\n"));
    assert!(!text.contains("-界"));
    assert!(
        workspace
            .perform(
                "ssmcp.fs.write_text",
                json!({"path":"notes/中文.txt","text":"stale","overwrite":true,
                    "expected_sha256":original["sha256"]}),
            )
            .await
            .is_err()
    );
    assert_eq!(
        std::fs::read_to_string(scratch.0.join("notes/中文.txt")).unwrap(),
        "changed\n界\n"
    );
    assert!(
        workspace
            .perform(
                "ssmcp.fs.write_text",
                json!({"path":"../escape.txt","text":"outside"}),
            )
            .await
            .is_err()
    );

    write(&workspace, "AGENTS.md", "Root instruction.\n").await;
    write(&workspace, "notes/AGENTS.md", "Lower priority.\n").await;
    write(
        &workspace,
        "notes/AGENTS.override.md",
        "Nested instruction.\n",
    )
    .await;
    let skill = "---\nname: release-check\ndescription: Check a native release.\n---\nSKILL_BODY_NOT_IN_INDEX\n";
    write(&workspace, ".agents/skills/release-check/SKILL.md", skill).await;
    let context = Context::new(scratch.0.clone(), vec![]).unwrap();
    let listing = context
        .perform("ssmcp.skills.list", json!({}))
        .await
        .unwrap();
    assert_eq!(listing["total"], 1);
    assert_eq!(listing["skills"][0]["name"], "release-check");
    let summary = context
        .perform("ssmcp.skills.view", json!({}))
        .await
        .unwrap();
    let markdown = summary["markdown"].as_str().unwrap();
    assert!(markdown.contains("release-check"));
    assert!(!markdown.contains("SKILL_BODY_NOT_IN_INDEX"));
    let document = context
        .perform(
            "ssmcp.skills.read",
            json!({"path":".agents/skills/release-check/SKILL.md"}),
        )
        .await
        .unwrap();
    assert_eq!(document["content"], skill);
    let instructions = context
        .perform("ssmcp.agents.resolve", json!({"path":"notes/new.rs"}))
        .await
        .unwrap();
    let files = instructions["files"].as_array().unwrap();
    assert_eq!(files.len(), 2);
    assert_eq!(files[0]["content"], "Root instruction.\n");
    assert_eq!(files[1]["path"], "notes/AGENTS.override.md");
    assert_eq!(files[1]["content"], "Nested instruction.\n");
    assert_eq!(instructions["target_exists"], false);
    assert!(
        context
            .perform("ssmcp.agents.read", json!({"path":"../AGENTS.md"}))
            .await
            .is_err()
    );
    drop(context);
    workspace.shutdown().await;
}

fn fixture_command() -> Vec<String> {
    vec![
        std::env::current_exe()
            .unwrap()
            .into_os_string()
            .into_string()
            .unwrap(),
        "--exact".into(),
        "fixture_process".into(),
        "--nocapture".into(),
    ]
}

async fn finished(shell: &Shell, job: &Value) -> Value {
    timeout(Duration::from_secs(15), async {
        loop {
            let status = shell
                .call(
                    "ssmcp.exec.wait",
                    json!({"job_id":job["job_id"],"wait_ms":1000}),
                )
                .await
                .unwrap();
            if status["running"] == false {
                return status;
            }
        }
    })
    .await
    .expect("native background command did not finish")
}

#[tokio::test]
async fn native_exec_preserves_arguments_environment_cwd_streams_and_exit_code() {
    let scratch = Scratch::new();
    let shell =
        Shell::with_workspace(scratch.0.clone(), OutputStore::new_connection().unwrap()).unwrap();
    let job = shell
        .call(
            "ssmcp.exec.start",
            json!({
                "command":fixture_command(),"cwd":scratch.0,"timeout_ms":10000,
                "env":{"SSMCP_RELEASE_FIXTURE":"emit","SSMCP_RELEASE_VALUE":"value with spaces 界"}
            }),
        )
        .await
        .unwrap();
    let status = finished(&shell, &job).await;
    assert_eq!(status["exit_code"], 23);
    assert_eq!(status["cancelled"], false);
    assert_eq!(status["timed_out"], false);
    let output = shell
        .call("ssmcp.exec.read", json!({"job_id":job["job_id"]}))
        .await
        .unwrap();
    assert!(output["stdout"].as_str().unwrap().contains("NATIVE_STDOUT"));
    assert!(output["stderr"].as_str().unwrap().contains("NATIVE_STDERR"));
    assert!(!output["stdout"].as_str().unwrap().contains("NATIVE_STDERR"));
    assert_eq!(
        std::fs::read_to_string(scratch.0.join("observed.txt")).unwrap(),
        "value with spaces 界"
    );
    let again = shell
        .call(
            "ssmcp.exec.read",
            json!({"job_id":job["job_id"],"stdout_cursor":output["stdout_cursor"],
                "stderr_cursor":output["stderr_cursor"]}),
        )
        .await
        .unwrap();
    assert_eq!(again["stdout"], "");
    assert_eq!(again["stderr"], "");
    shell.shutdown().await;
}

#[tokio::test]
async fn native_exec_cancel_stops_a_live_process() {
    let shell = Shell::new();
    let job = shell
        .call(
            "ssmcp.exec.start",
            json!({"command":fixture_command(),"timeout_ms":60000,
                "env":{"SSMCP_RELEASE_FIXTURE":"wait"}}),
        )
        .await
        .unwrap();
    timeout(Duration::from_secs(10), async {
        let mut output = String::new();
        let mut cursor = 0;
        loop {
            let chunk = shell
                .call(
                    "ssmcp.exec.read",
                    json!({"job_id":job["job_id"],"stdout_cursor":cursor,"wait_ms":500}),
                )
                .await
                .unwrap();
            cursor = chunk["stdout_cursor"].as_u64().unwrap();
            output.push_str(chunk["stdout"].as_str().unwrap());
            if output.contains("NATIVE_WAIT_READY") {
                break;
            }
        }
    })
    .await
    .expect("native child did not start");
    let cancelled = shell
        .call("ssmcp.exec.cancel", json!({"job_id":job["job_id"]}))
        .await
        .unwrap();
    assert_eq!(cancelled["running"], false);
    assert_eq!(cancelled["cancelled"], true);
    shell.shutdown().await;
}

/// Re-exec this test binary instead of relying on Bash or Python on Windows.
#[test]
fn fixture_process() {
    match std::env::var("SSMCP_RELEASE_FIXTURE").as_deref() {
        Ok("emit") => {
            std::fs::write(
                "observed.txt",
                std::env::var("SSMCP_RELEASE_VALUE").unwrap(),
            )
            .unwrap();
            println!("NATIVE_STDOUT");
            eprintln!("NATIVE_STDERR");
            std::io::stdout().flush().unwrap();
            std::process::exit(23);
        }
        Ok("wait") => {
            println!("NATIVE_WAIT_READY");
            std::io::stdout().flush().unwrap();
            std::thread::sleep(Duration::from_secs(60));
        }
        _ => {}
    }
}
