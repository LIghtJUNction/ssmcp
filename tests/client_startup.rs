//! Executable startup regression: isolated fake SSH and delayed native protocols.
//! No SSH server, credentials, browser, or model is used.
#![cfg(target_os = "linux")]

use serde_json::Value;
use std::{
    os::unix::{ffi::OsStrExt, fs::PermissionsExt},
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{process::Command, time::timeout};

const SSH: &str = r#"#!/usr/bin/python3
import json, os, pathlib, sys, time
directory = pathlib.Path(os.environ["SSMCP_STARTUP_DIR"])
scenario = os.environ["SSMCP_STARTUP_SCENARIO"]
assert json.loads(sys.stdin.readline())["type"] == "hello"
print(json.dumps({"type":"ready", "url":"https://ssmcp.example/alice",
                  "login_code":"a"*64, "login_expires_in":600,
                  "client_id":"b"*64}), flush=True)
deadline = time.monotonic() + 5
while not (directory / "native-request").exists():
    assert time.monotonic() < deadline, "native startup was never reached"
    time.sleep(0.005)
if scenario == "healthy":
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
directory = pathlib.Path(os.environ["SSMCP_STARTUP_DIR"])
scenario = os.environ["SSMCP_STARTUP_SCENARIO"]
phase = os.environ["SSMCP_STARTUP_PHASE"]
(directory / "native-pid").write_text(str(os.getpid()))
for line in sys.stdin:
    request = json.loads(line)
    if request.get("type") == "get_state":
        (directory / "native-request").write_text("startup")
        time.sleep(1.2 if scenario == "healthy" else 0.4)
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
        response = {"jsonrpc":"2.0", "id":request["id"], "result":{"tools":[]}}
    else:
        continue
    print(json.dumps(response), flush=True)
"#;

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("ssmcp-startup-{}-{unique}", std::process::id()));
        std::fs::create_dir(&path).unwrap();
        for (name, source) in [("ssh", SSH), ("native", NATIVE)] {
            let file = path.join(name);
            std::fs::write(&file, source).unwrap();
            std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        Self(path)
    }

    async fn run(&self, scenario: &str, phase: &str) -> std::process::Output {
        let mut paths = vec![self.0.clone()];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        let mut command = Command::new(env!("CARGO_BIN_EXE_ssmcp"));
        command
            .args([
                "-t",
                if phase == "discovery" { "cc" } else { "pi" },
                "--backend",
            ])
            .arg(self.0.join("native"))
            .arg("alice@fixture")
            .env("PATH", std::env::join_paths(paths).unwrap())
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
        let pid = std::fs::read_to_string(self.0.join("native-pid")).unwrap();
        timeout(Duration::from_secs(2), async {
            while running(&pid) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("native startup process survived client shutdown");
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
async fn ssh_eof_or_partial_frame_during_native_startup_never_publishes_a_login_code() {
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
async fn ssh_eof_during_native_tool_discovery_stops_the_backend_without_publishing() {
    let fixture = Fixture::new();
    let output = fixture.run("eof", "discovery").await;
    assert!(!output.status.success());
    assert!(output.stdout.is_empty(), "published revoked credentials");
    assert_eq!(
        std::fs::read_to_string(fixture.0.join("native-request")).unwrap(),
        "discovery"
    );
    fixture.assert_native_stopped().await;
}

#[tokio::test]
async fn live_ssh_preserves_partial_frames_and_deducts_backend_startup_time() {
    let fixture = Fixture::new();
    let output = fixture.run("healthy", "startup").await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let ready: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(ready["type"], "pi");
    assert_eq!(ready["client_id"], "b".repeat(64));
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
            .any(|tool| tool["name"] == "pi.get_state")
    );
    fixture.assert_native_stopped().await;
}
