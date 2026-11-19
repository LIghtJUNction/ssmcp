use super::*;

#[test]
fn recovery_errors_keep_utf8_boundaries_and_a_small_response() {
    let value = section(Err("界".repeat(1000)));
    assert_eq!(value["available"], false);
    assert!(value["error"].as_str().unwrap().len() <= 512);
    assert!(serde_json::to_vec(&value).unwrap().len() < 1024);
    assert_eq!(tool()["annotations"]["readOnlyHint"], true);
}

// The production storage handles are process-local. Use a private child test
// process instead of mutating HOME/XDG state for concurrently running tests.
#[cfg(unix)]
#[test]
fn recovery_native_tool_reads_current_sources_without_replaying_or_copying_them() {
    const MARKER: &str = "SSMCP_RECOVERY_ISOLATED_TEST";
    if std::env::var_os(MARKER).as_deref() == Some(std::ffi::OsStr::new("child")) {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(native_fixture());
        return;
    }
    let directory = std::env::temp_dir()
        .canonicalize()
        .unwrap()
        .join(crate::messaging::random_id("recovery_fixture").unwrap());
    crate::platform::private_directory(&directory).unwrap();
    let result = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "runtime::recovery::tests::recovery_native_tool_reads_current_sources_without_replaying_or_copying_them", "--nocapture"])
        .current_dir(&directory)
        .env(MARKER, "child")
        .env("HOME", &directory)
        .env("XDG_STATE_HOME", directory.join("state"))
        .env("XDG_CONFIG_HOME", directory.join("config"))
        .env_remove("SSMCP_CLIENT_CONFIG")
        .env_remove("SSMCP_SESSION_HOME")
        .env_remove("SSMCP_AUTH_HOME")
        .env_remove("DISPLAY")
        .env_remove("ZTTY_SOCKET")
        .output();
    let _ = std::fs::remove_dir_all(&directory);
    let result = result.unwrap();
    assert!(
        result.status.success(),
        "isolated recovery fixture failed:\n{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}

#[cfg(unix)]
async fn native_fixture() {
    use crate::{
        backend::Backend,
        client::{context_memory, interface::history},
        output::OutputStore,
        wire::Operation,
    };
    use futures_util::FutureExt;
    use std::path::Path;

    let root = std::env::current_dir().unwrap();
    let directory = history::default_directory().unwrap();
    assert!(!directory.exists());
    assert!(history::recovery(root.to_str().unwrap(), 8).is_err());
    assert!(context_memory::recovery(root.to_str().unwrap()).is_err());
    assert!(
        !directory.exists(),
        "observation initialized history storage"
    );
    context_memory::initialize().unwrap();
    history::record(
        "assistant",
        "Goal: keep parser behavior; the parser checks passed.",
    )
    .unwrap();
    history::record("assistant", &"下一步验证重连，不重放旧命令。".repeat(80)).unwrap();
    context_memory::call(
        "ssmcp.context.compact",
        json!({"summary":"Checkpoint: parser checked; next verify reconnect."}),
    )
    .unwrap();
    let original = history::current().unwrap().unwrap();
    let history_path = history::default_directory()
        .unwrap()
        .join(format!("{}.jsonl", original.metadata.id));
    let memory_path = history::default_directory()
        .unwrap()
        .parent()
        .unwrap()
        .join("context-memory")
        .join(format!(
            "{:x}",
            Sha256::digest(root.to_string_lossy().as_bytes())
        ))
        .join("state.json");
    let history_before = std::fs::read(&history_path).unwrap();
    let memory_before = std::fs::read(&memory_path).unwrap();
    let output = OutputStore::at_for_test(root.join("outputs")).unwrap();
    let backend = Backend::with_output(Path::new("/bin/sh"), root.clone(), output)
        .await
        .unwrap();
    let runtime = match Runtime::new(backend.clone(), root.clone()) {
        Ok(runtime) => runtime,
        Err(error) => {
            backend.shutdown().await;
            panic!("runtime: {error}");
        }
    };
    let outcome =
        std::panic::AssertUnwindSafe(async {
            let started = runtime.perform(Operation::CallTool {
            name:"ssmcp.exec.start".into(),
            arguments:json!({"command":["/bin/sh","-c","printf 'job-body-marker'; read value"],
                "title":"Owned recovery fixture","stdin":"pipe","timeout_ms":5000}),
        }).await.unwrap();
            let id = started["structuredContent"]["job_id"]
                .as_str()
                .unwrap()
                .to_owned();
            let before_jobs = backend.recovery_jobs(16)["total"].clone();
            let value = runtime
                .perform(Operation::CallTool {
                    name: "ssmcp.client.recover".into(),
                    arguments: json!({}),
                })
                .await
                .unwrap();
            assert_ne!(value["isError"], true);
            let recovered = &value["structuredContent"];
            assert_eq!(recovered["schema"], "ssmcp.session-recovery.v1");
            assert_eq!(recovered["identity"]["pid"], std::process::id());
            assert_eq!(recovered["history"]["available"], true);
            assert_eq!(
                recovered["history"]["value"]["conversation_id"],
                original.metadata.id
            );
            assert_eq!(
                recovered["history"]["value"]["last_message_id"],
                original.messages.last().unwrap().id
            );
            assert_eq!(recovered["checkpoint"]["available"], true);
            assert_eq!(
                recovered["checkpoint"]["value"]["summary"],
                "Checkpoint: parser checked; next verify reconnect."
            );
            assert_eq!(recovered["jobs"]["jobs"][0]["job_id"], id);
            assert_eq!(recovered["jobs"]["jobs"][0]["running"], true);
            assert_eq!(recovered["semantics"]["persistent_history_copy"], false);
            let serialized = serde_json::to_vec(recovered).unwrap();
            assert!(serialized.len() <= MAX_RESPONSE_BYTES);
            assert!(
                !String::from_utf8(serialized)
                    .unwrap()
                    .contains("job-body-marker")
            );
            assert_eq!(backend.recovery_jobs(16)["total"], before_jobs);
            assert_eq!(std::fs::read(&history_path).unwrap(), history_before);
            assert_eq!(std::fs::read(&memory_path).unwrap(), memory_before);
            for arguments in [
                Value::Null,
                json!({"recent_limit":null}),
                json!({"recent_limit":17}),
                json!({"job_limit":17}),
                json!({"resume":true}),
            ] {
                assert!(runtime.recover(arguments).await.is_err());
            }
            let empty = runtime
                .recover(json!({"recent_limit":0,"job_limit":0}))
                .await
                .unwrap();
            assert!(
                empty["history"]["value"]["recent"]
                    .as_array()
                    .unwrap()
                    .is_empty()
            );
            assert_eq!(empty["jobs"]["shown"], 0);
            assert_eq!(empty["jobs"]["total"], before_jobs);
            let other_root = root.join("other-workspace");
            crate::platform::private_directory(&other_root).unwrap();
            let other = Runtime::new(backend.clone(), other_root).unwrap();
            let outside = other.recover(json!({})).await.unwrap();
            assert_eq!(outside["history"]["available"], false);
            assert_eq!(outside["checkpoint"]["available"], false);
            assert_eq!(
                outside["identity"]["process_id"],
                recovered["identity"]["process_id"]
            );
            assert_ne!(
                outside["identity"]["session_id"],
                recovered["identity"]["session_id"]
            );
        })
        .catch_unwind()
        .await;
    runtime.shutdown().await;
    backend.shutdown().await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}
