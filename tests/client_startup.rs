//! Executable startup regression: isolated fake SSH and delayed native protocols.
//! No SSH server, credentials, browser, or model is used.
#![cfg(target_os = "linux")]

use serde_json::Value;
use std::{
    os::unix::{ffi::OsStrExt, fs::PermissionsExt},
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{process::Command, time::timeout};

const SSH: &str = r#"#!/usr/bin/python3
import json, os, pathlib, sys, time
print("warning: fixture stderr",file=sys.stderr,flush=True)
directory = pathlib.Path(os.environ["SSMCP_STARTUP_DIR"])
scenario = os.environ["SSMCP_STARTUP_SCENARIO"]
hello = json.loads(sys.stdin.readline())
assert hello["type"] == "hello"
(directory / "hello.json").write_text(json.dumps(hello))
github = scenario.startswith("github-")
code = "" if github else "a"*64
expires = 0 if github else 600
if scenario == "github-legacy":
    code, expires = "a"*64, 600
elif scenario == "github-bad-ttl":
    expires = 600
print(json.dumps({"type":"ready", "url":"https://ssmcp.example/mcp" if github else "https://ssmcp.example/alice",
                  "login_code":code, "login_expires_in":expires,
                  "client_id":"b"*64}), flush=True)
deadline = time.monotonic() + 5
while not (directory / "native-request").exists():
    assert time.monotonic() < deadline, "native startup was never reached"
    time.sleep(0.005)
if scenario.endswith("healthy"):
    # A partial frame must survive native startup and tool discovery.
    frame = json.dumps({"type":"call", "id":7, "operation":{"op":"list_tools"}}) + "\n"
    split = len(frame) // 2
    sys.stdout.write(frame[:split]); sys.stdout.flush()
    time.sleep(0.1)
    sys.stdout.write(frame[split:]); sys.stdout.flush()
    reply = json.loads(sys.stdin.readline())
    (directory / "reply.json").write_text(json.dumps(reply))
elif scenario == "partial-eof":
    sys.stdout.write('{"type":'); sys.stdout.flush()
# Closing SSH here must cancel a pending native initialization/tools/list.
"#;

const NATIVE: &str = r#"#!/usr/bin/python3
import json, os, pathlib, sys, time
print("warning: fixture stderr",file=sys.stderr,flush=True)
directory = pathlib.Path(os.environ["SSMCP_STARTUP_DIR"])
scenario = os.environ["SSMCP_STARTUP_SCENARIO"]
phase = os.environ["SSMCP_STARTUP_PHASE"]
(directory / "native-pid").write_text(str(os.getpid()))
for line in sys.stdin:
    request = json.loads(line)
    if request.get("type") == "get_state":
        (directory / "native-request").write_text("startup")
        time.sleep(1.2 if scenario.endswith("healthy") else 0.4)
        response = {"type":"response", "id":request["id"], "command":"get_state",
                    "success":True, "data":{}}
    elif request.get("method") == "initialize":
        assert phase == "discovery"
        response = {"jsonrpc":"2.0", "id":request["id"], "result":{
                    "protocolVersion":"2025-11-25", "capabilities":{"tools":{}},
                    "serverInfo":{"name":"startup-fixture", "version":"1"}}}
    elif request.get("method") == "tools/list":
        (directory / "native-request").write_text("discovery")
        time.sleep(0.4)
        if scenario == "catalog-error-healthy":
            response = {"jsonrpc":"2.0", "id":request["id"],
                        "error":{"code":-32000, "message":"fixture catalog unavailable"}}
        else:
            response = {"jsonrpc":"2.0", "id":request["id"], "result":{"tools":[]}}
    else:
        continue
    print(json.dumps(response), flush=True)
"#;

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "ssmcp-startup-{}-{unique}-{sequence}",
            std::process::id()
        ));
        std::fs::create_dir(&path).unwrap();
        for (name, source) in [("ssh", SSH), ("native", NATIVE)] {
            let file = path.join(name);
            std::fs::write(&file, source).unwrap();
            std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        Self(path)
    }

    async fn run(&self, scenario: &str, phase: &str) -> std::process::Output {
        self.run_with_args(scenario, phase, &[]).await
    }

    async fn run_with_args(
        &self,
        scenario: &str,
        phase: &str,
        extra_args: &[&str],
    ) -> std::process::Output {
        let harness = if phase == "discovery" { "claude" } else { "pi" };
        std::os::unix::fs::symlink(self.0.join("native"), self.0.join(harness)).unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_ssmcp"));
        std::fs::write(self.0.join("native-request"), "shell-started").unwrap();
        command
            .args(["--shell", "/bin/sh"])
            .args(extra_args)
            .arg("alice@fixture")
            // Exercise automatic discovery without observing real installed harnesses.
            .env("PATH", &self.0)
            .env("SSMCP_STARTUP_DIR", &self.0)
            .env("SSMCP_STARTUP_SCENARIO", scenario)
            .env("SSMCP_STARTUP_PHASE", phase)
            .kill_on_drop(true);
        timeout(Duration::from_secs(5), command.output())
            .await
            .expect("ssmcp did not stop after SSH ended")
            .unwrap()
    }

    async fn assert_native_stopped(&self) {
        assert!(
            !self.0.join("native-pid").exists(),
            "shell-only startup must not spawn a selected native backend"
        );
    }
}

fn running(pid: &str) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|stat| {
            stat.rsplit_once(')')
                .map(|(_, tail)| tail.trim_start().starts_with('Z'))
        })
        .is_some_and(|zombie| !zombie)
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // A failed regression must not strand its fixture. Confirm the unique
        // executable path still belongs to this PID before touching it.
        if let Ok(pid) = std::fs::read_to_string(self.0.join("native-pid"))
            && let Ok(number) = pid.parse::<i32>()
            && let Ok(command) = std::fs::read(format!("/proc/{number}/cmdline"))
            && command
                .split(|byte| *byte == 0)
                .any(|argument| argument == self.0.join("native").as_os_str().as_bytes())
        {
            // SAFETY: number names the fixture with our unique executable path.
            unsafe {
                libc::kill(number, libc::SIGKILL);
            }
        }
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::test]
async fn ssh_eof_or_partial_frame_during_shell_startup_never_publishes_credentials() {
    for scenario in ["eof", "partial-eof"] {
        let fixture = Fixture::new();
        let output = fixture.run(scenario, "startup").await;
        assert!(!output.status.success(), "closed SSH reported success");
        assert!(output.stdout.is_empty(), "published revoked credentials");
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains("SSH"), "{error}");
        assert!(!error.contains("Connected ("));
        fixture.assert_native_stopped().await;
    }
}

#[tokio::test]
async fn ssh_eof_during_shell_tool_discovery_never_publishes_credentials() {
    let fixture = Fixture::new();
    let output = fixture.run("eof", "discovery").await;
    assert!(!output.status.success());
    assert!(output.stdout.is_empty(), "published revoked credentials");
    assert_eq!(
        std::fs::read_to_string(fixture.0.join("native-request")).unwrap(),
        "shell-started"
    );
    fixture.assert_native_stopped().await;
}

#[tokio::test]
async fn live_ssh_preserves_partial_frames_and_publishes_shell_capabilities() {
    let fixture = Fixture::new();
    let output = fixture.run("healthy", "startup").await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let ready: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(ready["type"], "shell");
    assert_eq!(ready["client_id"], "b".repeat(64));
    assert!(ready.get("auth").is_none());
    assert!(ready.get("allow").is_none());
    assert_eq!(ready["login_code"].as_str().unwrap().len(), 64);
    assert!((1..600).contains(&ready["login_expires_in"].as_u64().unwrap()));
    let reply: Value =
        serde_json::from_slice(&std::fs::read(fixture.0.join("reply.json")).unwrap()).unwrap();
    assert_eq!(reply["type"], "reply");
    assert_eq!(reply["id"], 7);
    assert!(
        reply["result"]["Ok"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"] == "ssmcp.exec.start")
    );
    fixture.assert_native_stopped().await;
}

#[tokio::test]
async fn an_unavailable_optional_harness_catalog_keeps_shell_tools_online() {
    let fixture = Fixture::new();
    let output = fixture.run("catalog-error-healthy", "discovery").await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let ready: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(ready["type"], "shell");
    let reply: Value =
        serde_json::from_slice(&std::fs::read(fixture.0.join("reply.json")).unwrap()).unwrap();
    let tools = reply["result"]["Ok"]["tools"].as_array().unwrap();
    assert!(tools.iter().any(|tool| tool["name"] == "terminal.status"));
    assert!(tools.iter().any(|tool| tool["name"] == "ssmcp.exec.start"));
    assert!(
        tools
            .iter()
            .all(|tool| !tool["name"].as_str().unwrap().starts_with("cc."))
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("fixture catalog unavailable"));
    fixture.assert_native_stopped().await;
}

#[tokio::test]
async fn session_title_reaches_ssh_handshake_and_connection_output() {
    let fixture = Fixture::new();
    let title = "更新客户端 · 保留任务上下文";
    let output = fixture
        .run_with_args("healthy", "startup", &["--title", title])
        .await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let ready: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(ready["title"], title);
    let hello: Value =
        serde_json::from_slice(&std::fs::read(fixture.0.join("hello.json")).unwrap()).unwrap();
    assert_eq!(hello["metadata"]["title"], title);
    fixture.assert_native_stopped().await;
}

#[tokio::test]
async fn github_allow_handshake_publishes_browser_auth_without_login_code() {
    let fixture = Fixture::new();
    let output = fixture
        .run_with_args(
            "github-healthy",
            "startup",
            &[
                "--allow",
                "OctoCat@GitHub, Alice@github",
                "--allow",
                "OCTOCAT@github",
            ],
        )
        .await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let ready: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(ready["auth"], "github");
    assert_eq!(ready["url"], "https://ssmcp.example/mcp");
    assert_eq!(
        ready["allow"],
        serde_json::json!(["octocat@github", "alice@github"])
    );
    assert_eq!(ready["client_id"], "b".repeat(64));
    assert!(ready.get("login_code").is_none());
    assert!(ready.get("login_expires_in").is_none());
    let hello: Value =
        serde_json::from_slice(&std::fs::read(fixture.0.join("hello.json")).unwrap()).unwrap();
    assert_eq!(hello["version"], 4);
    assert_eq!(hello["metadata"]["allow"], ready["allow"]);
    assert!(String::from_utf8_lossy(&output.stderr).contains("allowed GitHub account"));
    fixture.assert_native_stopped().await;
}

#[tokio::test]
async fn github_allow_fails_closed_before_backend_start_if_server_returns_legacy_credentials() {
    for scenario in ["github-legacy", "github-bad-ttl"] {
        let fixture = Fixture::new();
        let output = fixture
            .run_with_args(scenario, "startup", &["--allow", "alice@github"])
            .await;
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(!fixture.0.join("native-pid").exists());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains("did not acknowledge the --allow"), "{error}");
        assert!(!error.contains(&"a".repeat(64)));
    }
}

#[tokio::test]
async fn jsonl_keeps_native_ssh_and_lifecycle_stderr_machine_readable() {
    let fixture = Fixture::new();
    let output = fixture
        .run_with_args(
            "github-healthy",
            "startup",
            &[
                "--allow",
                "octocat@github",
                "--log-format",
                "jsonl",
                "--no-color",
            ],
        )
        .await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(!stderr.contains('\x1b'));
    let events: Vec<Value> = stderr
        .lines()
        .map(|line| serde_json::from_str(line).expect("every stderr line is JSON"))
        .collect();
    assert!(
        events
            .iter()
            .any(|v| v["type"] == "ssmcp.status" && v["level"] == "Connected")
    );
    assert!(events.iter().any(|v| {
        v["type"] == "stderr"
            && v["text"]
                .as_str()
                .is_some_and(|s| s.contains("fixture stderr"))
    }));
    assert!(events.iter().any(|v| {
        v["stderr"]
            .as_str()
            .is_some_and(|s| s.contains("fixture stderr"))
    }));
    let connection: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(connection["auth"], "github");
    fixture.assert_native_stopped().await;
}
