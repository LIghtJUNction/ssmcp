use super::*;
use crate::{
    backend::Backend,
    output::OutputStore,
    runtime::{
        Runtime,
        hooks::{ToolCall, ToolCallHook},
    },
    wire::Operation,
};

fn descriptor(name: &str) -> Value {
    json!({"name":name,"inputSchema":{"type":"object","$defs":{"Keep":{"type":"string"}}},
        "outputSchema":{"type":"object"},"annotations":{"readOnlyHint":true},"x-extra":[1,"原生"]})
}

#[test]
fn catalog_preserves_native_schema_and_rejects_invalid_or_duplicate_routes() {
    let original = descriptor("echo");
    let prepared = prepare_catalog(
        "local",
        &json!({"schema_version":1,"tools":[original.clone()]}).to_string(),
    )
    .unwrap();
    let mut expected = original.clone();
    expected["name"] = "plugins.local.echo".into();
    assert_eq!(prepared.tools, vec![expected]);
    assert_eq!(prepared.routes["plugins.local.echo"], "echo");
    for text in [
        json!({"schema_version":2,"tools":[original.clone()]}).to_string(),
        json!({"schema_version":1,"tools":[original.clone(),original.clone()]}).to_string(),
        json!({"schema_version":1,"tools":[descriptor("ssmcp.catalog.v1")]}).to_string(),
        json!({"schema_version":1,"tools":[{"name":"bad","inputSchema":false}]}).to_string(),
        json!({"schema_version":1,"tools":vec![original; MAX_TOOLS+1]}).to_string(),
        "x".repeat(MAX_OUTPUT + 1),
    ] {
        assert!(prepare_catalog("local", &text).is_err());
    }
    assert!(prepare_catalog("../bad", r#"{"schema_version":1,"tools":[]}"#).is_err());
}

#[test]
fn rejected_plugin_catalog_preserves_old_handles_and_other_providers() {
    let registry = Registry::new();
    registry
        .replace(Provider::Workspace, vec![descriptor("ssmcp.fs.fixture")])
        .unwrap();
    registry
        .replace(Provider::Plugins, vec![descriptor("plugins.good.echo")])
        .unwrap();
    let old = registry.lookup("plugins.good.echo").unwrap();
    let generation = registry.generation();
    let before = registry.catalog().unwrap();
    for bad in [
        vec![json!({"name":"plugins.bad.no_schema"})],
        vec![
            descriptor("plugins.same.echo"),
            descriptor("plugins.same.echo"),
        ],
        vec![descriptor("ssmcp.fs.fixture")],
        vec![descriptor("plugins..missing_namespace")],
    ] {
        assert!(registry.replace_preserving(Provider::Plugins, bad).is_err());
        assert_eq!(registry.catalog().unwrap(), before);
        assert_eq!(registry.generation(), generation);
        registry.validate(&old).unwrap();
    }
}

#[test]
fn plugin_result_preserves_business_errors_and_rejects_bad_envelopes() {
    let result = json!({"content":[],"isError":true,"structuredContent":{"reason":"fixture"},"x-extra":"kept"});
    assert_eq!(parse_result(&result.to_string()).unwrap(), result);
    for text in [
        "{}",
        "[]",
        r#"{"content":false}"#,
        r#"{"content":[],"isError":"yes"}"#,
        "{",
    ] {
        assert!(parse_result(text).is_err());
    }
}

#[tokio::test]
async fn native_plugin_default_is_empty_without_even_resolving_a_path() {
    let registry = Registry::new();
    registry
        .replace(Provider::Plugins, Plugins::tools())
        .unwrap();
    let plugins = Plugins::new(
        PathBuf::from("/nonexistent/native-plugin-fixture-root"),
        registry.clone(),
    );
    let list = registry.lookup("ssmcp.plugins.list").unwrap();
    let result = plugins.perform(&list, json!({})).await.unwrap();
    assert_eq!(result["structuredContent"]["plugins"], json!([]));
    assert!(plugins.state.lock().unwrap().slots.is_empty());
    plugins.shutdown().await.unwrap();
}

#[test]
fn pinned_library_attempt_budget_remains_bounded_after_workers_stop() {
    let used = AtomicUsize::new(0);
    for _ in 0..MAX_NATIVE_LOADS {
        reserve_load(&used).unwrap();
    }
    assert_eq!(used.load(Ordering::Acquire), MAX_NATIVE_LOADS);
    assert!(reserve_load(&used).is_err());
    assert_eq!(used.load(Ordering::Acquire), MAX_NATIVE_LOADS);
}

#[test]
fn process_worker_permits_outlive_all_manager_handles() {
    let active = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        let mut owners = [Vec::new(), Vec::new()];
        let mut exits = Vec::new();
        let mut joins = Vec::new();
        let mut flags = Vec::new();
        for index in 0..MAX_PLUGINS {
            let (sender, _receiver) = mpsc::channel(QUEUE);
            let running = Arc::new(AtomicBool::new(true));
            let worker = Arc::new(Worker {
                sender,
                stop: Arc::new(AtomicBool::new(false)),
                running: running.clone(),
            });
            let guard = Running {
                running: running.clone(),
                _permit: WorkerPermit::reserve(&active).unwrap(),
            };
            let (exit, blocked_native_call) = std::sync::mpsc::channel::<()>();
            joins.push(scope.spawn(move || {
                let _running = guard;
                // A native call may ignore a manager's cancellation request.
                let _ = blocked_native_call.recv();
            }));
            owners[index % 2].push(worker);
            flags.push(running);
            exits.push(exit);
        }
        assert_eq!(active.load(Ordering::Acquire), MAX_PLUGINS);
        for owner in &owners {
            for worker in owner {
                worker.stop();
            }
        }
        drop(owners);
        assert!(flags.iter().all(|flag| flag.load(Ordering::Acquire)));
        assert!(WorkerPermit::reserve(&active).is_err());
        exits.remove(0).send(()).unwrap();
        joins.remove(0).join().unwrap();
        assert!(!flags[0].load(Ordering::Acquire));
        assert_eq!(active.load(Ordering::Acquire), MAX_PLUGINS - 1);
        let replacement = WorkerPermit::reserve(&active).unwrap();
        assert_eq!(active.load(Ordering::Acquire), MAX_PLUGINS);
        drop(replacement);
        for exit in exits {
            exit.send(()).unwrap();
        }
        for join in joins {
            join.join().unwrap();
        }
    });
    assert_eq!(active.load(Ordering::Acquire), 0);
}

#[test]
fn exhausted_library_budget_releases_unstarted_worker_reservation() {
    let active = AtomicUsize::new(0);
    let loads = AtomicUsize::new(MAX_NATIVE_LOADS);
    let start = || -> Result<(), String> {
        let _permit = WorkerPermit::reserve(&active)?;
        reserve_load(&loads)?;
        Ok(())
    };
    assert!(start().is_err());
    assert_eq!(active.load(Ordering::Acquire), 0);
    assert_eq!(loads.load(Ordering::Acquire), MAX_NATIVE_LOADS);
}

#[test]
fn cancelled_load_lease_stops_its_owned_worker_without_a_native_call() {
    let (sender, mut receiver) = mpsc::channel(QUEUE);
    let worker = Arc::new(Worker {
        sender,
        stop: Arc::new(AtomicBool::new(false)),
        running: Arc::new(AtomicBool::new(false)),
    });
    let lease = LoadLease(Some(worker.clone()));
    drop(lease);
    assert!(worker.stop.load(Ordering::Acquire));
    assert!(matches!(receiver.try_recv(), Ok(Command::Stop)));
}

async fn call(runtime: &Runtime, name: &str, arguments: Value) -> Result<Value, String> {
    runtime
        .perform(Operation::CallTool {
            name: name.into(),
            arguments,
        })
        .await
}
struct Deny;
impl ToolCallHook for Deny {
    fn before_call(&self, call: &ToolCall) -> Result<(), String> {
        if call.name == "plugins.demo.denied" {
            Err("fixture hook denial".into())
        } else {
            Ok(())
        }
    }
}
async fn exercise(root: &Path, library: &Path) {
    let shell = if cfg!(windows) {
        std::env::var_os("ComSpec")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(
                    std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into()),
                )
                .join("System32")
                .join("cmd.exe")
            })
    } else {
        PathBuf::from("/bin/sh")
    };
    let backend = Backend::with_output(
        &shell,
        root.to_owned(),
        OutputStore::new_connection().unwrap(),
    )
    .await
    .unwrap();
    let runtime = Runtime::new(backend, root.to_owned()).unwrap();
    assert!(runtime.plugins.state.lock().unwrap().slots.is_empty());
    let baseline = runtime.registry.catalog().unwrap();
    let listing = call(&runtime, "ssmcp.plugins.list", json!({}))
        .await
        .unwrap();
    assert_eq!(listing["structuredContent"]["plugins"], json!([]));
    assert_eq!(runtime.registry.catalog().unwrap(), baseline);
    assert!(
        call(
            &runtime,
            "ssmcp.plugins.load",
            json!({"path":"missing-native-file","namespace":"missing"})
        )
        .await
        .is_err()
    );
    assert_eq!(runtime.registry.catalog().unwrap(), baseline);

    let loaded = call(
        &runtime,
        "ssmcp.plugins.load",
        json!({"path":library,"namespace":"demo"}),
    )
    .await
    .unwrap();
    assert_eq!(loaded["structuredContent"]["status"], "ready");
    let tool = runtime.registry.lookup("plugins.demo.echo").unwrap();
    assert_eq!(tool.descriptor["x-fixture"]["unicode"], "原生");
    assert!(tool.descriptor["outputSchema"].is_object());
    assert_eq!(tool.descriptor["annotations"]["idempotentHint"], false);
    for original in &baseline {
        let name = original["name"].as_str().unwrap();
        assert_eq!(&runtime.registry.lookup(name).unwrap().descriptor, original);
    }
    let echo = call(&runtime, "plugins.demo.echo", json!({"text":"中文🙂"}))
        .await
        .unwrap();
    assert_eq!(echo["structuredContent"]["arguments"]["text"], "中文🙂");
    assert_eq!(echo["structuredContent"]["calls"], 1);
    let again = call(
        &runtime,
        "ssmcp.plugins.load",
        json!({"path":library,"namespace":"demo"}),
    )
    .await
    .unwrap();
    assert_eq!(again["structuredContent"]["status"], "ready");
    assert_eq!(runtime.plugins.state.lock().unwrap().slots.len(), 1);

    let accepted = runtime.registry.catalog().unwrap();
    let generation = runtime.registry.generation();
    assert!(
        call(
            &runtime,
            "ssmcp.plugins.load",
            json!({"path":library,"namespace":"duplicate"})
        )
        .await
        .unwrap_err()
        .contains("duplicate plugin tool")
    );
    assert_eq!(runtime.registry.catalog().unwrap(), accepted);
    assert_eq!(runtime.registry.generation(), generation);
    runtime.registry.validate(&tool).unwrap();

    runtime.set_tool_call_hook(Some(Arc::new(Deny)));
    assert!(
        call(&runtime, "plugins.demo.denied", json!({}))
            .await
            .unwrap_err()
            .contains("fixture hook denial")
    );
    let stats = call(&runtime, "plugins.demo.stats", json!({}))
        .await
        .unwrap();
    assert_eq!(
        stats["structuredContent"]["calls"], 1,
        "rejected plugin operation actually ran"
    );
    runtime.set_tool_call_hook(None);
    assert_eq!(
        call(&runtime, "plugins.demo.business_error", json!({}))
            .await
            .unwrap()["isError"],
        true
    );
    assert!(
        call(&runtime, "plugins.demo.invalid_utf8", json!({}))
            .await
            .is_err()
    );
    assert!(
        call(&runtime, "plugins.demo.oversized", json!({}))
            .await
            .is_err()
    );
    let large = call(&runtime, "plugins.demo.large", json!({}))
        .await
        .unwrap();
    assert_eq!(large["structuredContent"]["spilled"], true);
    call(
        &runtime,
        "ssmcp.plugins.unload",
        json!({"namespace":"demo"}),
    )
    .await
    .unwrap();
    assert!(runtime.registry.validate(&tool).is_err());
    assert!(
        call(&runtime, "plugins.demo.echo", json!({}))
            .await
            .is_err()
    );
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let stopped = {
                let state = runtime.plugins.state.lock().unwrap();
                !state.slots["demo"].worker.running.load(Ordering::Acquire)
            };
            if stopped {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    call(
        &runtime,
        "ssmcp.plugins.load",
        json!({"path":library,"namespace":"demo"}),
    )
    .await
    .unwrap();
    assert!(
        runtime.registry.validate(&tool).is_err(),
        "reloaded route reused a stale registration"
    );
    runtime.shutdown().await;
    assert!(runtime.registry.ensure_open().is_err());
    assert!(
        runtime
            .plugins
            .state
            .lock()
            .unwrap()
            .slots
            .values()
            .all(|slot| !slot.worker.running.load(Ordering::Acquire))
    );
}

struct Temporary(PathBuf);
impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// Cargo keeps examples in the profile directory, but test executables may use
// either the legacy deps directory or the namespaced build/<crate>/<hash>/out layout.
fn native_plugin_fixture_path(executable: &Path) -> Option<PathBuf> {
    let artifacts = executable.parent()?;
    let profile = if artifacts.ends_with("deps") {
        artifacts.parent()?
    } else {
        let build = artifacts.ancestors().nth(3)?;
        if !artifacts.ends_with("out") || !build.ends_with("build") {
            return None;
        }
        build.parent()?
    };
    Some(profile.join("examples").join(format!(
        "{}native_plugin_fixture{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    )))
}

#[test]
fn native_plugin_fixture_supports_cargo_artifact_layouts() {
    let library = format!(
        "{}native_plugin_fixture{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    );
    for profile in [
        "target/debug",
        "target/aarch64-apple-darwin/release",
        "custom-target/x86_64-pc-windows-msvc/custom-profile",
    ] {
        let profile = Path::new(profile);
        let expected = profile.join("examples").join(&library);
        for relative in ["deps/ssmcp-test", "build/ssmcp/hash/out/ssmcp-test"] {
            assert_eq!(
                native_plugin_fixture_path(&profile.join(relative)),
                Some(expected.clone())
            );
        }
    }
    assert_eq!(
        native_plugin_fixture_path(Path::new("unrelated/hash/out/ssmcp-test")),
        None
    );
}

#[test]
#[ignore = "requires cargo build --example native_plugin_fixture"]
fn native_plugin_runtime_contract() {
    const CHILD: &str = "SSMCP_NATIVE_PLUGIN_CONTRACT_ROOT";
    const LIBRARY: &str = "SSMCP_NATIVE_PLUGIN_FIXTURE_LIBRARY";
    if let Some(root) = std::env::var_os(CHILD) {
        let library =
            PathBuf::from(std::env::var_os(LIBRARY).expect("fixture library not supplied"));
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(exercise(Path::new(&root), &library));
        return;
    }
    let executable = std::env::current_exe().unwrap();
    let library =
        native_plugin_fixture_path(&executable).expect("unrecognized Cargo test executable layout");
    assert!(
        library.is_file(),
        "missing real plugin fixture {}; run cargo build --example native_plugin_fixture with the same target/profile first",
        library.display()
    );
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let temporary = Temporary(std::env::temp_dir().join(format!(
        "ssmcp-native-plugin-{}-{unique}",
        std::process::id()
    )));
    for name in [
        "workspace",
        "home",
        "state",
        "config",
        "data",
        "cache",
        "tmp",
        "bin",
    ] {
        std::fs::create_dir_all(temporary.0.join(name)).unwrap();
    }
    let root = temporary.0.join("workspace");
    let local = root.join(format!("插件-fixture{}", std::env::consts::DLL_SUFFIX));
    std::fs::copy(&library, &local).unwrap();
    let output = std::process::Command::new(&executable)
        .args([
            "--exact",
            "runtime::plugins::tests::native_plugin_runtime_contract",
            "--ignored",
            "--nocapture",
        ])
        .current_dir(&root)
        .env(CHILD, &root)
        .env(LIBRARY, &local)
        .env("PATH", temporary.0.join("bin"))
        .env("HOME", temporary.0.join("home"))
        .env("XDG_STATE_HOME", temporary.0.join("state"))
        .env("XDG_CONFIG_HOME", temporary.0.join("config"))
        .env("XDG_DATA_HOME", temporary.0.join("data"))
        .env("XDG_CACHE_HOME", temporary.0.join("cache"))
        .env("TMPDIR", temporary.0.join("tmp"))
        .env("SSMCP_AUTH_HOME", temporary.0.join("state/auth"))
        .env("SSMCP_SESSION_HOME", temporary.0.join("state/sessions"))
        .env_remove("SSMCP_SESSION_GENERATION")
        .env_remove("SSMCP_CLIENT_OUTBOX")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
