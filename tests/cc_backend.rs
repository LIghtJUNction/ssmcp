//! Claude MCP adapter integration tests using an isolated local protocol fixture.
//! These tests neither launch the real Claude CLI nor call a paid model.
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use ssmcp::backend::cc::Claude;
use std::{
    ffi::{OsStr, OsString},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::time::{sleep, timeout};

struct Directory(PathBuf);

impl Directory {
    fn new() -> Self {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        Self::at(timestamp)
    }

    fn at(timestamp: u128) -> Self {
        // Parallel tests can observe the same clock tick even with nanosecond units.
        static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "ssmcp-cc-{}-{timestamp}-{sequence}",
            std::process::id()
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }

    async fn start(&self, additional: &[&str]) -> Arc<Claude> {
        let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cc-fake.py");
        let mut args = vec![
            OsString::from("--fixture-dir"),
            self.0.clone().into_os_string(),
        ];
        args.extend(additional.iter().map(OsString::from));
        timeout(
            Duration::from_secs(10),
            Claude::start(fixture.as_os_str(), &args),
        )
        .await
        .unwrap()
        .unwrap()
    }
}

impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn directories_with_the_same_clock_tick_remain_isolated() {
    let first = Directory::at(123_456);
    let second = Directory::at(123_456);
    assert_ne!(first.0, second.0);
    std::fs::write(first.0.join("owner"), "first").unwrap();
    std::fs::write(second.0.join("owner"), "second").unwrap();
    drop(first);
    assert_eq!(
        std::fs::read_to_string(second.0.join("owner")).unwrap(),
        "second"
    );
}

fn payload(result: Value) -> Value {
    result.get("structuredContent").cloned().unwrap_or_else(|| {
        serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap()
    })
}

async fn events(client: &Claude, cursor: u64, wait_ms: u64) -> Value {
    payload(
        client
            .call(
                "ssmcp.cc.events",
                json!({"cursor":cursor,"wait_ms":wait_ms,"max_events":256}),
            )
            .await
            .unwrap(),
    )
}

#[tokio::test]
async fn dynamic_paginated_tools_and_native_results_are_preserved() {
    let directory = Directory::new();
    let client = directory
        .start(&["--model", "fixture-model", "--verbose"])
        .await;
    let tools = client.tools().await.unwrap();
    let catalogue: Vec<Value> =
        serde_json::from_str(include_str!("fixtures/cc-catalog.json")).unwrap();
    for mut expected in catalogue {
        expected["name"] = json!(format!("cc.{}", expected["name"].as_str().unwrap()));
        assert_eq!(
            tools
                .iter()
                .find(|tool| tool["name"] == expected["name"])
                .unwrap(),
            &expected
        );
    }
    assert!(tools.iter().any(|tool| tool["name"] == "cc.Dynamic1"));
    assert!(tools.iter().any(|tool| tool["name"] == "ssmcp.cc.events"));
    assert!(tools.iter().any(|tool| tool["name"] == "cc.events"));
    assert!(
        tools
            .iter()
            .any(|tool| tool["name"] == "cc.ssmcp.cc.respond")
    );
    let arguments = json!({"nested":[3,5,8],"error":true});
    let result = client
        .call("cc.NativeFutureTool", arguments.clone())
        .await
        .unwrap();
    assert_eq!(
        result,
        json!({
            "content":[
                {"type":"text","text":"native result"},
                {"type":"image","mimeType":"image/png","data":"AAEC"},
                {"type":"resource","resource":{"uri":"fixture://native-result","mimeType":"text/plain","text":"embedded"}}
            ],
            "structuredContent":{"future":{"arguments":arguments}},
            "isError":true,
            "_meta":{"vendor/result":[1,2,3]},
            "futureResultField":{"keep":"this too"}
        })
    );
    let refreshed = client.tools().await.unwrap();
    assert!(refreshed.iter().any(|tool| tool["name"] == "cc.Dynamic2"));
    assert!(!refreshed.iter().any(|tool| tool["name"] == "cc.Dynamic1"));
    let response = client
        .call("cc.ssmcp.cc.respond", json!({"anything":true}))
        .await
        .unwrap();
    assert_eq!(
        response["structuredContent"]["native_name"],
        "ssmcp.cc.respond"
    );
    let page = events(&client, 0, 0).await;
    assert!(
        page["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| event["event"]["method"] == "notifications/progress")
    );
    let argv: Value =
        serde_json::from_slice(&std::fs::read(directory.0.join("argv.json")).unwrap()).unwrap();
    assert_eq!(argv[0], "mcp");
    assert_eq!(argv[1], "serve");
    assert_eq!(argv[4], "--model");
    assert_eq!(argv[5], "fixture-model");
    assert_eq!(argv[6], "--verbose");
    client.shutdown().await;
}

#[tokio::test]
async fn native_requests_wait_for_an_explicit_response() {
    let directory = Directory::new();
    let client = directory.start(&[]).await;
    let calling = client.clone();
    let task = tokio::spawn(async move { calling.call("cc.NeedsApproval", json!({})).await });
    let pending = timeout(Duration::from_secs(5), async {
        loop {
            let pending = payload(client.call("ssmcp.cc.requests", json!({})).await.unwrap());
            if !pending["requests"].as_array().unwrap().is_empty() {
                break pending;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(pending["requests"][0]["id"], "approval-1");
    assert_eq!(pending["requests"][0]["method"], "elicitation/create");
    assert!(!task.is_finished());
    let wire = std::fs::read_to_string(directory.0.join("received.jsonl")).unwrap();
    assert!(
        !wire
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .any(|message| message["id"] == "approval-1")
    );
    assert!(
        client
            .call(
                "ssmcp.cc.respond",
                json!({"id":"approval-1","result":{},"error":{"code":-1,"message":"no"}})
            )
            .await
            .is_err()
    );
    client
        .call(
            "ssmcp.cc.respond",
            json!({"id":"approval-1","result":{"action":"decline"}}),
        )
        .await
        .unwrap();
    let result = timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        result["structuredContent"]["approval"],
        json!({"jsonrpc":"2.0","id":"approval-1","result":{"action":"decline"}})
    );
    assert!(
        client
            .call("ssmcp.cc.respond", json!({"id":"approval-1","result":{}}))
            .await
            .is_err()
    );
    assert!(
        payload(client.call("ssmcp.cc.requests", json!({})).await.unwrap())["requests"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    client.shutdown().await;
}

#[tokio::test]
async fn background_output_is_incremental_and_shutdown_stops_jobs_and_watchers() {
    let directory = Directory::new();
    let client = directory.start(&[]).await;
    #[cfg(target_os = "linux")]
    let (child, _owned) = {
        client
            .call("cc.NativeFutureTool", json!({"spawn_child":true}))
            .await
            .unwrap();
        let child: i32 = std::fs::read_to_string(directory.0.join("child-pid"))
            .unwrap()
            .parse()
            .unwrap();
        // Keep the fixture child bound by pidfd so an assertion failure also cleans it up.
        let owned = OwnedJobs::for_session(child);
        assert!(owned.0.iter().any(|(pid, _)| *pid == child));
        (child, owned)
    };
    client
        .call("cc.Bash", json!({"fixture_mode":"background"}))
        .await
        .unwrap();
    let mut cursor = 0;
    let mut bytes = Vec::new();
    timeout(Duration::from_secs(5), async {
        while bytes != b"first\nsecond\n" {
            let page = events(&client, cursor, 1000).await;
            cursor = page["next_cursor"].as_u64().unwrap();
            for entry in page["events"].as_array().unwrap() {
                let event = &entry["event"];
                if event["type"] == "background_output" {
                    assert_eq!(event["offset"].as_u64().unwrap(), bytes.len() as u64);
                    bytes.extend(
                        STANDARD
                            .decode(event["data_base64"].as_str().unwrap())
                            .unwrap(),
                    );
                }
            }
        }
    })
    .await
    .unwrap();
    let quiet = events(&client, cursor, 300).await;
    assert!(quiet["events"].as_array().unwrap().is_empty());
    client.shutdown().await;
    let stopped = events(&client, cursor, 0).await;
    assert_eq!(stopped["closed"], true);
    let next = stopped["next_cursor"].as_u64().unwrap();
    std::fs::write(
        directory.0.join("background.output"),
        b"must not be observed after shutdown",
    )
    .unwrap();
    sleep(Duration::from_millis(300)).await;
    assert!(
        events(&client, next, 300).await["events"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    #[cfg(target_os = "linux")]
    {
        let leader: i32 = std::fs::read_to_string(directory.0.join("pid"))
            .unwrap()
            .parse()
            .unwrap();
        assert!(!running(leader), "native MCP process survived shutdown");
        assert!(!running(child), "native child process survived shutdown");
    }
}

#[cfg(target_os = "linux")]
fn running(pid: i32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => !matches!(
            stat.rsplit_once(") ")
                .and_then(|(_, rest)| rest.chars().next()),
            Some('Z' | 'X')
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(_) => true,
    }
}

#[tokio::test]
async fn native_output_symlink_is_not_followed() {
    let directory = Directory::new();
    let client = directory.start(&[]).await;
    client
        .call("cc.Bash", json!({"fixture_mode":"symlink"}))
        .await
        .unwrap();
    let mut cursor = 0;
    let mut seen = Vec::new();
    timeout(Duration::from_secs(5), async {
        loop {
            let page = events(&client, cursor, 1000).await;
            cursor = page["next_cursor"].as_u64().unwrap();
            seen.extend(page["events"].as_array().unwrap().iter().cloned());
            if seen
                .iter()
                .any(|entry| entry["event"]["type"] == "background_output_error")
            {
                break;
            }
        }
    })
    .await
    .unwrap();
    assert!(
        !seen
            .iter()
            .any(|entry| entry["event"]["type"] == "background_output")
    );
    assert!(
        !serde_json::to_string(&seen)
            .unwrap()
            .contains("DO_NOT_TAIL")
    );
    client.shutdown().await;
}

#[tokio::test]
async fn pagination_loops_and_transport_overrides_fail_with_clear_errors() {
    let directory = Directory::new();
    let client = directory.start(&["--repeat-cursor"]).await;
    assert!(client.tools().await.unwrap_err().contains("repeated"));
    client.shutdown().await;
    for flag in [
        "-p",
        "--print",
        "--output-format=json",
        "--input-format=stream-json",
        "--sdk-url=ws://localhost",
    ] {
        let error = Claude::start(
            OsStr::new("/ssmcp-this-program-must-not-launch"),
            &[OsString::from(flag)],
        )
        .await;
        assert!(error.err().unwrap().to_string().contains("transport"));
    }
}

/// Opt-in smoke against the official installed binary. Configuration and all
/// environment credentials are isolated by the wrapper; no Agent/model call is
/// made. Example:
/// SSMCP_TEST_CLAUDE=/absolute/path/claude cargo test --test cc_backend \
///     real_claude_backend_smoke -- --ignored --nocapture
#[cfg(unix)]
#[tokio::test]
#[ignore = "requires an explicitly supplied official Claude Code executable"]
async fn real_claude_backend_smoke() {
    use std::os::unix::fs::PermissionsExt;
    let program = std::env::var("SSMCP_TEST_CLAUDE")
        .expect("set SSMCP_TEST_CLAUDE to an absolute Claude Code executable path");
    assert!(std::path::Path::new(&program).is_absolute());
    let directory = Directory::new();
    let configuration = directory.0.join("claude-config");
    std::fs::create_dir(&configuration).unwrap();
    let marker = directory.0.join("marker.txt");
    std::fs::write(&marker, b"SSMCP_BACKEND_READ\n").unwrap();
    let configuration_path = directory.0.join("wrapper.json");
    std::fs::write(
        &configuration_path,
        json!({"program":program,"cwd":directory.0,"config":configuration}).to_string(),
    )
    .unwrap();
    let wrapper = directory.0.join("claude-isolated.py");
    let source = format!(
        "#!/usr/bin/env python3\nimport json, os, sys\nwith open({}) as f:\n    c = json.load(f)\nos.chdir(c['cwd'])\nenv = {{'PATH': '/usr/bin:/bin', 'SHELL': '/bin/bash', 'LANG': 'C.UTF-8', 'CLAUDE_CONFIG_DIR': c['config'], 'CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC': '1', 'ENABLE_CLAUDEAI_MCP_SERVERS': 'false'}}\nos.execve(c['program'], [c['program'], *sys.argv[1:]], env)\n",
        serde_json::to_string(&configuration_path).unwrap()
    );
    std::fs::write(&wrapper, source).unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
    let client = timeout(
        Duration::from_secs(30),
        Claude::start(wrapper.as_os_str(), &[]),
    )
    .await
    .unwrap()
    .unwrap();
    let tools = client.tools().await.unwrap();
    eprintln!(
        "real Claude Code exposed {} native tools and 3 bridge controls",
        tools.len() - 3
    );
    assert!(tools.iter().any(|tool| tool["name"] == "cc.Read"));
    assert!(tools.iter().any(|tool| tool["name"] == "cc.Bash"));
    let read = client
        .call("cc.Read", json!({"file_path":marker}))
        .await
        .unwrap();
    let read: Value = serde_json::from_str(read["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(read["file"]["content"], "SSMCP_BACKEND_READ\n");
    let result = client
        .call(
            "cc.Bash",
            json!({
                "command":"printf 'SSMCP_BACKEND_BEGIN\\n'; sleep 1; printf 'SSMCP_BACKEND_END\\n'",
                "description":"Print delayed backend smoke test markers",
                "run_in_background":true
            }),
        )
        .await
        .unwrap();
    let result: Value =
        serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
    assert!(
        result["backgroundOutputPath"].is_string(),
        "requires Claude Code >= 2.1.295"
    );
    let mut cursor = 0;
    let mut output = Vec::new();
    timeout(Duration::from_secs(10), async {
        while output != b"SSMCP_BACKEND_BEGIN\nSSMCP_BACKEND_END\n" {
            let page = events(&client, cursor, 1000).await;
            cursor = page["next_cursor"].as_u64().unwrap();
            for entry in page["events"].as_array().unwrap() {
                let event = &entry["event"];
                if event["type"] == "background_output" {
                    assert_eq!(event["offset"].as_u64().unwrap(), output.len() as u64);
                    output.extend(
                        STANDARD
                            .decode(event["data_base64"].as_str().unwrap())
                            .unwrap(),
                    );
                }
            }
        }
    })
    .await
    .unwrap();
    #[cfg(target_os = "linux")]
    {
        // Claude Code 2.1.325 starts background Bash in another Linux session.
        // Its TaskStop currently reports "No task found" for these native MCP jobs,
        // so verify cleanup of an actual live Bash and its sleep child via pidfds.
        let live = client
            .call(
                "cc.Bash",
                json!({
                    "command":"printf '%s\\n' \"$BASHPID\"; sleep 60 & wait",
                    "description":"Exercise owned background process cleanup",
                    "run_in_background":true
                }),
            )
            .await
            .unwrap();
        let live: Value =
            serde_json::from_str(live["content"][0]["text"].as_str().unwrap()).unwrap();
        let output = PathBuf::from(live["backgroundOutputPath"].as_str().unwrap());
        let bash_pid = timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(output) = std::fs::read_to_string(&output)
                    && let Ok(pid) = output.trim().parse::<i32>()
                {
                    break pid;
                }
                sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        sleep(Duration::from_millis(100)).await;
        let jobs = OwnedJobs::for_session(process_session(bash_pid).unwrap());
        assert!(jobs.0.iter().any(|(pid, _)| *pid == bash_pid));
        assert!(
            jobs.0.len() >= 2,
            "expected a live Bash and its sleep child"
        );
        client.shutdown().await;
        timeout(Duration::from_secs(2), async {
            while !jobs.exited() {
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("live native Bash or its child survived backend shutdown");
    }
    #[cfg(not(target_os = "linux"))]
    client.shutdown().await;
    assert_eq!(events(&client, cursor, 0).await["closed"], true);
}

// A failed smoke assertion must not strand its own 60-second background job.
// Bind pidfds after validating this fixture's session, never signal a raw PID.
#[cfg(target_os = "linux")]
struct OwnedJobs(Vec<(i32, std::os::fd::OwnedFd)>);

#[cfg(target_os = "linux")]
impl OwnedJobs {
    fn for_session(session: i32) -> Self {
        use std::os::fd::FromRawFd;
        let mut owned = Vec::new();
        for entry in std::fs::read_dir("/proc").unwrap().flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|pid| pid.parse::<i32>().ok())
            else {
                continue;
            };
            if process_session(pid) != Some(session) {
                continue;
            }
            // SAFETY: pidfd_open receives a checked integer process id.
            let descriptor = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
            if descriptor < 0 {
                continue;
            }
            // SAFETY: successful pidfd_open returns a new owned descriptor.
            let descriptor = unsafe { std::os::fd::OwnedFd::from_raw_fd(descriptor as i32) };
            if process_session(pid) == Some(session) {
                owned.push((pid, descriptor));
            }
        }
        Self(owned)
    }

    fn exited(&self) -> bool {
        use std::os::fd::AsRawFd;
        self.0.iter().all(|(_, descriptor)| {
            let mut descriptor = libc::pollfd {
                fd: descriptor.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: poll receives one valid, writable descriptor entry.
            unsafe {
                libc::poll(&mut descriptor, 1, 0) == 1 && descriptor.revents & libc::POLLIN != 0
            }
        })
    }
}

#[cfg(target_os = "linux")]
impl Drop for OwnedJobs {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
        for (_, descriptor) in &self.0 {
            // SAFETY: each descriptor is bound to a verified fixture process;
            // the null siginfo requests ordinary SIGKILL delivery.
            unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    descriptor.as_raw_fd(),
                    libc::SIGKILL,
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                );
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn process_session(pid: i32) -> Option<i32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.rsplit_once(") ")?
        .1
        .split_whitespace()
        .nth(3)?
        .parse()
        .ok()
}
