//! Optional in-process extensions around validated native tool calls.
use crate::registry::Provider;
use serde_json::Value;

/// One normalized call. The id is local to this Runtime, not a wire request ID.
/// Arguments may contain private data; retain only what your harness needs.
#[derive(Debug)]
pub struct ToolCall {
    pub id: u64,
    pub name: String,
    pub native_name: String,
    pub provider: Provider,
    pub generation: u64,
    pub arguments: Value,
}

/// Trusted client code can reject calls and observe final output-guarded results.
/// Callbacks run without a hook lock and may overlap. Keep them nonblocking;
/// use your own bounded channel for slow work. They cannot replace arguments,
/// results, registrations, or permissions.
///
/// after_call also observes before_call rejections. Cancelling/dropping a call
/// before completion does not produce an after callback or undo side effects.
pub trait ToolCallHook: Send + Sync {
    fn before_call(&self, _call: &ToolCall) -> Result<(), String> {
        Ok(())
    }

    fn after_call(&self, _call: &ToolCall, _result: &Result<Value, String>) {}
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::{
        backend::Backend, output::OutputStore, registry::Registry, runtime::Runtime,
        wire::Operation, workspace::Workspace,
    };
    use serde_json::json;
    use std::{
        path::{Path, PathBuf},
        process::Command,
        sync::{Arc, Mutex},
        time::{SystemTime, UNIX_EPOCH},
    };

    struct Temporary(PathBuf);
    impl Drop for Temporary {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[derive(Debug)]
    struct Observation {
        path: String,
        error: Option<String>,
        success: bool,
        spilled: bool,
    }

    struct FixtureHook {
        registry: Arc<Registry>,
        observed: Mutex<Vec<Observation>>,
    }

    impl ToolCallHook for FixtureHook {
        fn before_call(&self, call: &ToolCall) -> Result<(), String> {
            assert_eq!(call.provider, Provider::Workspace);
            assert!(call.id > 0 && call.generation > 0);
            assert_eq!(call.name, call.native_name);
            match call.arguments["path"].as_str().unwrap() {
                "blocked.txt" => Err("fixture rejects this write".into()),
                "before-panic.txt" => panic!("fixture before callback"),
                "allowed.txt" => {
                    // Idempotent refreshes and unrelated providers must not
                    // revoke this workspace call's registered tool handle.
                    self.registry
                        .replace(Provider::Workspace, Workspace::tools())?;
                    self.registry.replace(
                        Provider::Context,
                        vec![json!({
                            "name":"ssmcp.context.hook_fixture",
                            "description":"unrelated provider update",
                            "inputSchema":{"type":"object","properties":{}}
                        })],
                    )?;
                    Ok(())
                }
                "stale.txt" => {
                    // Re-registering identical descriptors is intentionally a
                    // no-op. Change the actual tool selected for this call.
                    let mut tools = Workspace::tools();
                    let selected = tools
                        .iter_mut()
                        .find(|tool| tool["name"] == call.native_name)
                        .unwrap();
                    selected["description"] = json!("changed by the before fixture");
                    self.registry.replace(Provider::Workspace, tools)?;
                    Ok(())
                }
                _ => Ok(()),
            }
        }

        fn after_call(&self, call: &ToolCall, result: &Result<Value, String>) {
            let path = call.arguments["path"].as_str().unwrap().to_owned();
            if path == "after-panic.txt" {
                panic!("fixture after callback");
            }
            self.observed.lock().unwrap().push(Observation {
                path,
                error: result.as_ref().err().cloned(),
                success: result.as_ref().is_ok_and(|value| value["isError"] != true),
                spilled: result
                    .as_ref()
                    .is_ok_and(|value| value["structuredContent"]["spilled"] == true),
            });
        }
    }

    async fn write(runtime: &Runtime, path: &str) -> Result<Value, String> {
        runtime
            .perform(Operation::CallTool {
                name: "ssmcp.fs.write_text".into(),
                arguments: json!({"path":path,"text":"native fixture contents"}),
            })
            .await
    }

    async fn exercise(root: &Path) {
        let workspace = root.join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let backend = Backend::with_output(
            Path::new("/bin/sh"),
            workspace.clone(),
            OutputStore::new_connection().unwrap(),
        )
        .await
        .unwrap();
        let runtime = Runtime::new(backend, workspace.clone()).unwrap();
        let default = write(&runtime, "default.txt").await.unwrap();
        assert_ne!(default["isError"], true);
        assert_eq!(
            std::fs::read_to_string(workspace.join("default.txt")).unwrap(),
            "native fixture contents"
        );

        let hook = Arc::new(FixtureHook {
            registry: runtime.registry.clone(),
            observed: Mutex::new(Vec::new()),
        });
        runtime.set_tool_call_hook(Some(hook.clone()));
        let denied = write(&runtime, "blocked.txt").await.unwrap_err();
        assert!(denied.contains("fixture rejects this write"));
        assert!(!workspace.join("blocked.txt").exists());
        let allowed = write(&runtime, "allowed.txt").await.unwrap();
        assert_eq!(
            allowed["structuredContent"]["sha256"],
            default["structuredContent"]["sha256"]
        );
        assert_eq!(
            std::fs::read_to_string(workspace.join("allowed.txt")).unwrap(),
            "native fixture contents"
        );

        // An allowing hook never widens the native workspace boundary.
        assert!(write(&runtime, "../outside.txt").await.is_err());
        assert!(!root.join("outside.txt").exists());
        let panic = write(&runtime, "before-panic.txt").await.unwrap_err();
        assert!(panic.contains("panicked before dispatch"));
        assert!(!workspace.join("before-panic.txt").exists());
        // Changing a provider generation during before_call invalidates dispatch.
        assert!(write(&runtime, "stale.txt").await.is_err());
        assert!(!workspace.join("stale.txt").exists());
        // An observer cannot turn a completed write into a failed operation.
        assert!(write(&runtime, "after-panic.txt").await.is_ok());
        assert!(workspace.join("after-panic.txt").is_file());
        let audit = runtime.audit.read(json!({})).await.unwrap();
        assert!(audit["events"].as_array().unwrap().iter().any(|entry| {
            entry["event"]["type"] == "ssmcp.status"
                && entry["event"]["level"] == "Warning"
                && entry["event"]["message"]
                    .as_str()
                    .is_some_and(|message| message.contains("hook panicked after"))
        }));

        std::fs::write(workspace.join("large.bin"), vec![b'x'; 128 * 1024]).unwrap();
        let large = runtime
            .perform(Operation::CallTool {
                name: "ssmcp.fs.read".into(),
                arguments: json!({"path":"large.bin","max_bytes":128*1024}),
            })
            .await
            .unwrap();
        assert_eq!(large["structuredContent"]["spilled"], true);
        {
            let observed = hook.observed.lock().unwrap();
            assert_eq!(observed.len(), 6);
            assert!(
                observed.iter().any(|item| item.path == "blocked.txt"
                    && item.error.as_deref() == Some(denied.as_str()))
            );
            assert!(
                observed
                    .iter()
                    .any(|item| item.path == "allowed.txt" && item.success)
            );
            for path in ["../outside.txt", "before-panic.txt", "stale.txt"] {
                assert!(
                    observed
                        .iter()
                        .any(|item| item.path == path && item.error.is_some())
                );
            }
            assert!(
                observed
                    .iter()
                    .any(|item| item.path == "large.bin" && item.success && item.spilled)
            );
        }
        runtime.set_tool_call_hook(None);
        assert!(write(&runtime, "blocked.txt").await.is_ok());
        assert!(workspace.join("blocked.txt").exists());
        assert_eq!(hook.observed.lock().unwrap().len(), 6);
        runtime.shutdown().await;
    }

    #[test]
    fn hooks_guard_real_workspace_calls_and_preserve_native_outcomes() {
        const CHILD: &str = "SSMCP_RUNTIME_HOOK_TEST_ROOT";
        if let Some(root) = std::env::var_os(CHILD) {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(exercise(Path::new(&root)));
            return;
        }
        let root = Temporary(std::env::temp_dir().join(format!(
            "ssmcp-runtime-hooks-{}-{}", std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos(),
        )));
        for name in ["home", "state", "config", "data", "cache", "tmp", "bin"] {
            std::fs::create_dir_all(root.0.join(name)).unwrap();
        }
        // Separate process: no global environment races with parallel tests.
        // Empty PATH prevents discovery of the developer's installed harnesses.
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "runtime::hooks::tests::hooks_guard_real_workspace_calls_and_preserve_native_outcomes", "--nocapture"])
            .env(CHILD, &root.0).env("PATH", root.0.join("bin"))
            .env("HOME", root.0.join("home")).env("XDG_STATE_HOME", root.0.join("state"))
            .env("XDG_CONFIG_HOME", root.0.join("config")).env("XDG_DATA_HOME", root.0.join("data"))
            .env("XDG_CACHE_HOME", root.0.join("cache")).env("TMPDIR", root.0.join("tmp"))
            .output().unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
