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
phase = os.environ["SSMCP_STARTUP_PHASE"]
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

pending = {}
identifier = 6

def request(operation, *, partial=False, before_finish=None):
    global identifier
    identifier += 1
    frame = json.dumps({"type":"call", "id":identifier, "operation":operation}) + "\n"
    if partial:
        split = len(frame) // 2
        sys.stdout.write(frame[:split]); sys.stdout.flush()
        if before_finish is not None:
            before_finish()
        else:
            time.sleep(0.02)
        sys.stdout.write(frame[split:]); sys.stdout.flush()
    else:
        sys.stdout.write(frame); sys.stdout.flush()
    return identifier

def receive(wanted):
    if wanted in pending:
        return pending.pop(wanted)
    while True:
        line = sys.stdin.readline()
        assert line, "client closed SSH before delivering a reply"
        reply = json.loads(line)
        if reply["type"] == "steering":
            # Presence polling must remain independent of native calls.
            assert hello["version"] == 7, hello
            assert reply["command"] == {"op":"snapshot"}, reply
            print(json.dumps({"type":"steering_reply", "id":reply["id"], "result":{"Ok":{
                "schema_version":1, "client_id":"b"*64, "revision":0,
                "agents":[], "messages":[]}}}), flush=True)
            continue
        assert reply["type"] == "reply", reply
        if reply["id"] == wanted:
            return reply
        assert reply["id"] not in pending and len(pending) < 32, reply
        pending[reply["id"]] = reply

def data(reply):
    result = reply["result"]["Ok"]
    assert not result.get("isError", False), result
    return result.get("structuredContent", result)

def call(name, arguments=None):
    return data(receive(request({"op":"call_tool", "name":name, "arguments":arguments or {}})))

def catalog(*, partial=False, before_finish=None):
    first = receive(request({"op":"list_tools"}, partial=partial, before_finish=before_finish))
    page = data(first)
    tools = list(page["tools"])
    cursors = set()
    while page.get("nextCursor"):
        cursor = page["nextCursor"]
        assert cursor not in cursors and len(cursors) < 100, "invalid catalog pagination"
        cursors.add(cursor)
        page = data(receive(request({"op":"list_tools", "cursor":cursor})))
        tools.extend(page["tools"])
    assert len(tools) == len({tool["name"] for tool in tools}), "duplicate catalog entries"
    return first, tools

def wait_for(predicate, description):
    deadline = time.monotonic() + 4
    while not predicate():
        assert time.monotonic() < deadline, description
        time.sleep(0.005)

def assert_unloaded():
    rows = call("ssmcp.harnesses.list")["harnesses"]
    assert len(rows) == 5 and {row["provider"] for row in rows} == {
        "codex", "claude", "pi", "opencode", "playwright"}, rows
    assert all(row["status"] == "not_loaded" and row["tools"] == 0
               and "executable" in row and row["executable"] is None
               and "version" in row and row["version"] is None for row in rows), rows
    for name in ("version-probe", "native-pid", "native-pids"):
        assert not (directory / name).exists(), "observation launched an optional program: " + name
    return {"harnesses":rows}

wait_for(lambda: (directory / "native-request").exists(), "shell launch was never requested")
if scenario.endswith("healthy"):
    # Ordinary startup serves a fragmented catalog request without probing even
    # an installed fixture provider. Explicit cases establish this baseline too.
    first, baseline = catalog(partial=True)
    (directory / "reply.json").write_text(json.dumps(first))
    (directory / "catalog.json").write_text(json.dumps({"tools":baseline}))
    defaults = assert_unloaded()
    (directory / "defaults.json").write_text(json.dumps(defaults))
    shell = call("terminal.status")
    assert shell["running"] and shell["pid"] > 0, shell
    (directory / "shell-status.json").write_text(json.dumps(shell))
    assert_unloaded()
    explicit = scenario in ("native-healthy", "github-native-healthy",
                            "catalog-error-healthy", "catalog-timeout-healthy")
    if explicit:
        load_id = request({"op":"call_tool", "name":"ssmcp.harnesses.load",
                           "arguments":{"provider":"claude"}})
        # Keep another SSH frame partial until initialize/tools-list is blocked.
        # Completion of this independent request must not await the load reply.
        def awaiting_native():
            wait_for(lambda: (directory / "native-request").read_text() == phase,
                     "explicit native handshake was never reached")
        _, tools = catalog(partial=True, before_finish=awaiting_native)
        assert tools == baseline, "pending initialization altered the native-only catalog"
        (directory / "catalog.json").write_text(json.dumps({"tools":tools}))
        shell = call("terminal.status")
        assert shell["running"], shell
        (directory / "shell-status.json").write_text(json.dumps(shell))
        loading = call("ssmcp.harnesses.list")
        state = next(row for row in loading["harnesses"] if row["provider"] == "claude")
        assert state["status"] == "loading" and state["tools"] == 0, state
        (directory / "loading.json").write_text(json.dumps(loading))
        assert load_id not in pending, "load finished while its native response was gated"
        assert not (directory / "release-native").exists()
        assert not (directory / "native-response").exists()
        (directory / "catalog-before-native").write_text("confirmed")
        (directory / "release-native").write_text("continue")
        if scenario == "catalog-timeout-healthy":
            wait_for(lambda: (directory / "native-catalog-unanswered").exists(),
                     "native fixture never reached the deliberately unanswered catalog")
            settled = call("ssmcp.harnesses.list")
            state = next(row for row in settled["harnesses"] if row["provider"] == "claude")
            assert state["status"] == "loading", state
            assert load_id not in pending, "unanswered catalog unexpectedly completed loading"
            assert not (directory / "native-response").exists()
        else:
            loaded = receive(load_id)
            if scenario == "catalog-error-healthy":
                result = loaded["result"]
                assert "Err" in result or result.get("Ok", {}).get("isError") is True, loaded
                assert "fixture catalog unavailable" in json.dumps(loaded), loaded
                expected = "unavailable"
            else:
                data(loaded)
                expected = "ready"
            (directory / "load-reply.json").write_text(json.dumps(loaded))
            settled = call("ssmcp.harnesses.list")
            state = next(row for row in settled["harnesses"] if row["provider"] == "claude")
            assert state["status"] == expected, state
        assert all(row["status"] == "not_loaded" and row["tools"] == 0
                   and row["executable"] is None and row["version"] is None
                   for row in settled["harnesses"] if row["provider"] != "claude"), settled
        (directory / "harnesses.json").write_text(json.dumps(settled))
    else:
        (directory / "catalog-before-native").write_text("confirmed without loading")
        (directory / "harnesses.json").write_text(json.dumps(defaults))
elif scenario == "partial-eof":
    sys.stdout.write('{"type":'); sys.stdout.flush()
# Closing SSH cancels pending loading and stops every owned native process.
"#;

const NATIVE: &str = r#"#!/usr/bin/python3
import json, os, pathlib, sys, time
directory = pathlib.Path(os.environ["SSMCP_STARTUP_DIR"])
scenario = os.environ["SSMCP_STARTUP_SCENARIO"]
phase = os.environ["SSMCP_STARTUP_PHASE"]
with (directory / "native-pids").open("a") as pids:
    pids.write(str(os.getpid()) + "\n")
if sys.argv[1:] == ["--version"]:
    (directory / "version-probe").write_text("completed")
    print("startup-fixture 1.0.0", flush=True)
    raise SystemExit(0)
print("warning: fixture stderr",file=sys.stderr,flush=True)
(directory / "native-pid").write_text(str(os.getpid()))

def wait_for_release():
    deadline = time.monotonic() + 4
    while not (directory / "release-native").exists():
        assert time.monotonic() < deadline, "shell catalog was blocked by native discovery"
        time.sleep(0.005)

for line in sys.stdin:
    request = json.loads(line)
    if request.get("method") == "initialize":
        if phase == "startup":
            (directory / "native-request").write_text("startup")
            wait_for_release()
        response = {"jsonrpc":"2.0", "id":request["id"], "result":{
                    "protocolVersion":"2025-11-25", "capabilities":{"tools":{}},
                    "serverInfo":{"name":"startup-fixture", "version":"1"}}}
    elif request.get("method") == "tools/list":
        if phase == "discovery":
            (directory / "native-request").write_text("discovery")
            wait_for_release()
        if scenario == "catalog-timeout-healthy":
            # Keep the native process alive but never acknowledge a catalog request.
            (directory / "native-catalog-unanswered").write_text("waiting")
            continue
        if scenario == "catalog-error-healthy":
            response = {"jsonrpc":"2.0", "id":request["id"],
                        "error":{"code":-32000, "message":"fixture catalog unavailable"}}
        else:
            response = {"jsonrpc":"2.0", "id":request["id"], "result":{"tools":[]}}
    else:
        continue
    if ((phase == "startup" and request.get("method") == "initialize")
            or (phase == "discovery" and request.get("method") == "tools/list")):
        (directory / "native-response").write_text("released")
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
        std::fs::write(path.join("config.toml"), "").unwrap();
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
        // Explicit scenarios delay native initialize or tools/list on the same
        // controlled MCP fixture. Ordinary startup must not execute it at all.
        std::os::unix::fs::symlink(self.0.join("native"), self.0.join("claude")).unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_ssmcp"));
        std::fs::write(self.0.join("native-request"), "shell-started").unwrap();
        command
            .args(["--ssh", "--no-watchdog", "--shell", "/bin/sh"])
            .args(extra_args)
            .arg("alice@fixture")
            // Expose only the fixture: no real installed harness can be discovered.
            .env("PATH", &self.0)
            // Explicit SSH must not borrow the developer's saved account or
            // configured relay while exercising this local transport fixture.
            .env("SSMCP_AUTH_HOME", self.0.join("auth"))
            .env("SSMCP_CLIENT_CONFIG", self.0.join("config.toml"))
            .env("SSMCP_SESSION_HOME", self.0.join("sessions"))
            .env_remove("SSMCP_SESSION_GENERATION")
            .env_remove("SSMCP_CLIENT_OUTBOX")
            .env_remove("SSMCP_OUTPUT_RELOAD")
            .env_remove("SSMCP_UI_CONFIG")
            .env("SSMCP_STARTUP_DIR", &self.0)
            .env("SSMCP_STARTUP_SCENARIO", scenario)
            .env("SSMCP_STARTUP_PHASE", phase)
            .kill_on_drop(true);
        let limit = if scenario == "catalog-timeout-healthy" {
            Duration::from_secs(15)
        } else {
            Duration::from_secs(5)
        };
        timeout(limit, command.output())
            .await
            .expect("ssmcp did not stop after SSH ended")
            .unwrap()
    }

    fn assert_native_unloaded(&self) {
        for name in ["version-probe", "native-pid", "native-pids"] {
            assert!(
                !self.0.join(name).exists(),
                "native-only startup unexpectedly executed an optional program: {name}"
            );
        }
        assert_eq!(
            std::fs::read_to_string(self.0.join("native-request")).unwrap(),
            "shell-started"
        );
    }

    async fn assert_native_stopped(&self) {
        let pids = std::fs::read_to_string(self.0.join("native-pids")).unwrap_or_default();
        timeout(Duration::from_secs(2), async {
            while pids.lines().any(running) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("optional loading left an owned native process running");
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
        // A failed regression must not strand a probe or native service. Confirm
        // the unique fixture executable path before touching a recorded PID.
        if let Ok(pids) = std::fs::read_to_string(self.0.join("native-pids")) {
            for pid in pids.lines() {
                if let Ok(number) = pid.parse::<i32>()
                    && let Ok(command) = std::fs::read(format!("/proc/{number}/cmdline"))
                    && command.split(|byte| *byte == 0).any(|argument| {
                        ["native", "claude", "pi"]
                            .iter()
                            .any(|name| argument == self.0.join(name).as_os_str().as_bytes())
                    })
                {
                    // SAFETY: number names a process in our unique fixture.
                    unsafe {
                        libc::kill(number, libc::SIGKILL);
                    }
                }
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
    assert!(fixture.0.join("catalog-before-native").exists());
    assert_eq!(ready["login_code"].as_str().unwrap().len(), 64);
    assert!((1..=600).contains(&ready["login_expires_in"].as_u64().unwrap()));
    let reply: Value =
        serde_json::from_slice(&std::fs::read(fixture.0.join("reply.json")).unwrap()).unwrap();
    assert_eq!(reply["type"], "reply");
    assert_eq!(reply["id"], 7);
    let catalog: Value =
        serde_json::from_slice(&std::fs::read(fixture.0.join("catalog.json")).unwrap()).unwrap();
    let tools = catalog["tools"].as_array().unwrap();
    assert!(tools.iter().any(|tool| tool["name"] == "ssmcp.exec.start"));
    assert!(tools.iter().any(|tool| tool["name"] == "terminal.status"));
    assert!(
        tools
            .iter()
            .any(|tool| tool["name"] == "ssmcp.harnesses.load")
    );
    assert!(tools.iter().all(|tool| {
        !["codex.", "claude.", "pi.", "opencode.", "playwright."]
            .iter()
            .any(|prefix| tool["name"].as_str().unwrap().starts_with(prefix))
    }));
    fixture.assert_native_unloaded();
    fixture.assert_native_stopped().await;
}

#[tokio::test]
async fn explicit_optional_initialize_keeps_shell_catalog_and_status_online() {
    let fixture = Fixture::new();
    let output = fixture.run("native-healthy", "startup").await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let ready: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(ready["type"], "shell");
    assert!(fixture.0.join("catalog-before-native").exists());
    assert!(fixture.0.join("version-probe").exists());
    assert!(fixture.0.join("native-response").exists());
    let loading: Value =
        serde_json::from_slice(&std::fs::read(fixture.0.join("loading.json")).unwrap()).unwrap();
    assert!(
        loading["harnesses"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["provider"] == "claude" && entry["status"] == "loading")
    );
    let settled: Value =
        serde_json::from_slice(&std::fs::read(fixture.0.join("harnesses.json")).unwrap()).unwrap();
    assert!(
        settled["harnesses"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["provider"] == "claude" && entry["status"] == "ready")
    );
    let status: Value =
        serde_json::from_slice(&std::fs::read(fixture.0.join("shell-status.json")).unwrap())
            .unwrap();
    assert_eq!(status["running"], true);
    fixture.assert_native_stopped().await;
}

#[tokio::test]
async fn an_unavailable_optional_harness_catalog_keeps_shell_tools_online() {
    let fixture = Fixture::new();
    let output = fixture
        .run_with_args(
            "catalog-error-healthy",
            "discovery",
            &["--log-format", "jsonl", "--no-color"],
        )
        .await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let ready: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(ready["type"], "shell");
    let catalog: Value =
        serde_json::from_slice(&std::fs::read(fixture.0.join("catalog.json")).unwrap()).unwrap();
    let tools = catalog["tools"].as_array().unwrap();
    assert!(tools.iter().any(|tool| tool["name"] == "terminal.status"));
    assert!(tools.iter().any(|tool| tool["name"] == "ssmcp.exec.start"));
    assert!(
        tools
            .iter()
            .all(|tool| !tool["name"].as_str().unwrap().starts_with("claude."))
    );
    let diagnostics: Value =
        serde_json::from_slice(&std::fs::read(fixture.0.join("harnesses.json")).unwrap()).unwrap();
    assert!(
        diagnostics["harnesses"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["provider"] == "claude"
                && entry["status"] == "unavailable"
                && entry["reason"]
                    .as_str()
                    .is_some_and(|reason| reason.contains("fixture catalog unavailable")))
    );
    assert!(warning_contains(
        &output.stderr,
        "fixture catalog unavailable"
    ));
    fixture.assert_native_stopped().await;
}

fn warning_contains(stderr: &[u8], text: &str) -> bool {
    String::from_utf8_lossy(stderr).lines().any(|line| {
        serde_json::from_str::<Value>(line).is_ok_and(|event| {
            event["type"] == "ssmcp.status"
                && event["level"] == "Warning"
                && event["message"].as_str().is_some_and(|s| s.contains(text))
        })
    })
}

#[tokio::test]
async fn an_unresponsive_optional_catalog_keeps_shell_online_and_cancels_on_disconnect() {
    let fixture = Fixture::new();
    let output = fixture
        .run_with_args(
            "catalog-timeout-healthy",
            "discovery",
            &["--log-format", "jsonl"],
        )
        .await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let ready: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(ready["type"], "shell");
    assert_eq!(ready["client_id"], "b".repeat(64));
    let catalog: Value =
        serde_json::from_slice(&std::fs::read(fixture.0.join("catalog.json")).unwrap()).unwrap();
    let tools = catalog["tools"].as_array().unwrap();
    assert!(tools.iter().any(|tool| tool["name"] == "terminal.status"));
    assert!(tools.iter().any(|tool| tool["name"] == "ssmcp.exec.start"));
    assert!(
        tools
            .iter()
            .all(|tool| !tool["name"].as_str().unwrap().starts_with("claude."))
    );
    assert!(fixture.0.join("catalog-before-native").exists());
    assert!(!fixture.0.join("native-response").exists());
    let diagnostics: Value =
        serde_json::from_slice(&std::fs::read(fixture.0.join("harnesses.json")).unwrap()).unwrap();
    assert!(
        diagnostics["harnesses"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["provider"] == "claude" && entry["status"] == "loading")
    );
    // Every native request is still pending when SSH closes. The client must
    // cancel explicit loading without waiting for its full catalog timeout.
    for line in String::from_utf8_lossy(&output.stderr).lines() {
        serde_json::from_str::<Value>(line).expect("every stderr line is JSON");
    }
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
    fixture.assert_native_unloaded();
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
    assert_eq!(hello["version"], ssmcp::wire::VERSION);
    assert_eq!(hello["metadata"]["allow"], ready["allow"]);
    assert!(String::from_utf8_lossy(&output.stderr).contains("allowed GitHub account"));
    fixture.assert_native_unloaded();
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
        assert!(!fixture.0.join("version-probe").exists());
        assert!(!fixture.0.join("native-pids").exists());
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
            "github-native-healthy",
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
