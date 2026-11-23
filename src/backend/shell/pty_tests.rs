//! Real Linux PTY contracts exercised through the shared native job registry.
use super::*;

async fn start(shell: &Shell, command: Value) -> Value {
    shell
        .call("ssmcp.exec.start", json!({"mode":"pty","command":command}))
        .await
        .unwrap()
}

async fn wait(shell: &Shell, id: &str) -> Value {
    let result = shell
        .call("ssmcp.exec.wait", json!({"job_id":id,"wait_ms":5000}))
        .await
        .unwrap();
    assert_eq!(result["running"], false, "{result}");
    result
}

async fn until(shell: &Shell, id: &str, cursor: &mut u64, marker: &[u8]) -> Vec<u8> {
    timeout(Duration::from_secs(5), async {
        let mut bytes = Vec::new();
        loop {
            let page = shell
                .call(
                    "ssmcp.exec.read",
                    json!({
                        "job_id":id,"stdout_cursor":*cursor,"max_bytes":32768,"wait_ms":100,
                    }),
                )
                .await
                .unwrap();
            let delta = STANDARD
                .decode(page["stdout_base64"].as_str().unwrap())
                .unwrap();
            assert_eq!(
                page["stdout_cursor"].as_u64().unwrap(),
                *cursor + delta.len() as u64
            );
            *cursor = page["stdout_cursor"].as_u64().unwrap();
            bytes.extend_from_slice(&delta);
            if bytes.windows(marker.len()).any(|part| part == marker) {
                return bytes;
            }
            assert_ne!(
                page["stdout_eof"],
                true,
                "PTY ended before marker: {}",
                String::from_utf8_lossy(&bytes)
            );
        }
    })
    .await
    .expect("PTY marker deadline")
}

#[tokio::test]
async fn pty_binary_pages_keep_exact_byte_cursors_and_real_exit_status() {
    let shell = Shell::new();
    let script = r#"import os,tty
assert all(os.isatty(fd) for fd in (0,1,2))
assert os.tcgetpgrp(0)==os.getpgrp()
tty.setraw(0)
data=bytes(range(256))*257
while data:
    data=data[os.write(1,data):]
os.write(2,b'\x00\xffSTDERR')
raise SystemExit(7)
"#;
    let started = start(&shell, json!(["python3", "-c", script])).await;
    let id = started["job_id"].as_str().unwrap();
    assert_eq!(started["mode"], "pty");
    assert_eq!(started["stdin"]["mode"], "pty");
    assert_eq!(started["pid_role"], "command");
    let done = wait(&shell, id).await;
    assert_eq!(done["exit_code"], 7, "{done}");
    let expected: Vec<u8> = (0..257)
        .flat_map(|_| 0_u8..=255)
        .chain(b"\0\xffSTDERR".iter().copied())
        .collect();
    let mut cursor = 0;
    let mut received = Vec::new();
    loop {
        let page = shell
            .call(
                "ssmcp.exec.read",
                json!({"job_id":id,"stdout_cursor":cursor,"max_bytes":257}),
            )
            .await
            .unwrap();
        let bytes = STANDARD
            .decode(page["stdout_base64"].as_str().unwrap())
            .unwrap();
        assert!(bytes.len() <= 257);
        assert_eq!(page["stdout_truncated"], false);
        assert_eq!(page["stdout_cursor"], cursor + bytes.len() as u64);
        assert_eq!(page["stderr_cursor"], 0);
        assert_eq!(page["stderr_eof"], true);
        cursor += bytes.len() as u64;
        received.extend_from_slice(&bytes);
        if page["stdout_eof"] == true {
            break;
        }
        assert!(!bytes.is_empty());
    }
    assert_eq!(received, expected);
    // Legacy terminal reads retain their artifact-spill protocol. Exec reads of
    // the very same physical buffer must not inherit its jump-to-end cursor.
    let legacy = shell
        .call("terminal.read", json!({"terminal_id":id,"max_bytes":257}))
        .await
        .unwrap();
    assert_eq!(legacy["spilled"], true);
    assert_eq!(legacy["next_cursor"], cursor);
    assert!(
        shell
            .call("ssmcp.exec.read", json!({"job_id":id,"stderr_cursor":1}))
            .await
            .is_err()
    );
    shell
        .call("ssmcp.exec.forget", json!({"job_id":id}))
        .await
        .unwrap();
    assert!(
        shell
            .call("ssmcp.exec.status", json!({"job_id":id}))
            .await
            .is_err()
    );
    shell.shutdown().await;
}

#[tokio::test]
async fn terminal_and_exec_share_job_identity_control_signals_and_selection() {
    let shell = Shell::with_terminal(
        Path::new("/bin/sh"),
        std::env::current_dir().unwrap(),
        OutputStore::new_connection().unwrap(),
    )
    .unwrap();
    let main = shell
        .call("ssmcp.exec.status", json!({"job_id":"main"}))
        .await
        .unwrap();
    assert_eq!(main["pid"], shell.primary().pid());
    for name in ["ssmcp.exec.cancel", "ssmcp.exec.forget"] {
        assert!(shell.call(name, json!({"job_id":"main"})).await.is_err());
    }
    assert!(
        shell
            .call("terminal.close", json!({"terminal_id":"main"}))
            .await
            .is_err()
    );
    let opened = shell
        .call(
            "terminal.open",
            json!({"name":"same physical shell","select":true}),
        )
        .await
        .unwrap();
    let id = opened["job_id"].as_str().unwrap();
    assert_eq!(opened["terminal_id"], id);
    let job = shell.job(id).unwrap();
    assert!(Arc::ptr_eq(
        &shell.get(id).unwrap(),
        job.terminal.as_ref().unwrap()
    ));
    assert!(Arc::ptr_eq(
        &shell.selected(),
        job.terminal.as_ref().unwrap()
    ));
    let other = Shell::new();
    assert!(
        other
            .call("ssmcp.exec.status", json!({"job_id":id}))
            .await
            .is_err()
    );
    assert!(
        other
            .call(
                "terminal.write",
                json!({"terminal_id":id,"data":"must not run\n"})
            )
            .await
            .is_err()
    );
    let mut cursor = 0;
    shell
        .call(
            "ssmcp.exec.write",
            json!({"job_id":id,"data":"stty -echo; printf 'READY_'; printf 'ONE\\n'\n"}),
        )
        .await
        .unwrap();
    until(&shell, id, &mut cursor, b"READY_ONE").await;
    shell
        .call(
            "ssmcp.exec.resize",
            json!({"job_id":id,"rows":37,"cols":113}),
        )
        .await
        .unwrap();
    shell
        .call(
            "terminal.write",
            json!({"terminal_id":id,"data":"stty size\n"}),
        )
        .await
        .unwrap();
    until(&shell, id, &mut cursor, b"37 113").await;
    shell
        .call(
            "terminal.write",
            json!({"terminal_id":id,"data":"printf 'BUSY_NOW\\n'; sleep 30\n"}),
        )
        .await
        .unwrap();
    until(&shell, id, &mut cursor, b"BUSY_NOW").await;
    timeout(Duration::from_secs(3), async {
        loop {
            let foreground = std::fs::read_to_string(format!("/proc/{}/stat", job.pid))
                .ok()
                .and_then(|stat| {
                    stat.rsplit_once(')').and_then(|(_, tail)| {
                        tail.split_whitespace()
                            .nth(5)
                            .and_then(|value| value.parse::<u32>().ok())
                    })
                });
            if foreground.is_some_and(|pid| pid != 0 && pid != job.pid) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let interrupt = shell
        .call(
            "ssmcp.exec.write",
            json!({"job_id":id,"data_base64":STANDARD.encode([3])}),
        )
        .await
        .unwrap();
    assert_eq!(interrupt["written"], 1);
    shell
        .call(
            "ssmcp.exec.write",
            json!({"job_id":id,"data":"printf 'STILL_'; printf 'ALIVE\\n'\n"}),
        )
        .await
        .unwrap();
    until(&shell, id, &mut cursor, b"STILL_ALIVE").await;
    assert_eq!(
        shell
            .call("ssmcp.exec.status", json!({"job_id":id}))
            .await
            .unwrap()["running"],
        true
    );
    let closed = shell
        .call("terminal.close", json!({"terminal_id":id}))
        .await
        .unwrap();
    assert_eq!(closed["closed"], true);
    assert_eq!(closed["selected"], "main");
    assert!(!job.state().running);
    assert!(
        shell
            .call("ssmcp.exec.status", json!({"job_id":id}))
            .await
            .is_err()
    );
    assert_eq!(
        shell
            .call("terminal.close", json!({"terminal_id":id}))
            .await
            .unwrap()["closed"],
        false
    );
    other.shutdown().await;
    shell.shutdown().await;
}

#[tokio::test]
async fn pty_partial_write_receipt_matches_bytes_received_and_close_rejects_before_input() {
    let shell = Shell::new();
    let script = r#"import os,signal,tty,time,select
wake=False
def resume(*args):
    global wake
    wake=True
signal.signal(signal.SIGUSR1,resume)
tty.setraw(0)
os.write(1,b'READY')
while not wake:
    time.sleep(0.01)
os.set_blocking(0,False)
data=bytearray()
deadline=time.monotonic()+0.3
while time.monotonic()<deadline:
    if select.select([0],[],[],0.02)[0]:
        try:
            part=os.read(0,65536)
            if not part: break
            data.extend(part)
        except BlockingIOError: pass
os.write(1,('INPUT:%d:%s\n'%(len(data),all(n==80 for n in data))).encode())
"#;
    let started = start(&shell, json!(["python3", "-c", script])).await;
    let id = started["job_id"].as_str().unwrap();
    let mut cursor = 0;
    until(&shell, id, &mut cursor, b"READY").await;
    let mut written = 0_u64;
    let mut partial = false;
    for _ in 0..8 {
        let receipt = shell
            .call(
                "ssmcp.exec.write",
                json!({"job_id":id,"data":"P".repeat(MAX_INPUT),"wait_ms":20}),
            )
            .await
            .unwrap();
        let count = receipt["written"].as_u64().unwrap();
        assert_eq!(receipt["remaining_bytes"], MAX_INPUT as u64 - count);
        assert!(receipt["error"].is_null(), "{receipt}");
        written += count;
        if count < MAX_INPUT as u64 {
            assert_eq!(receipt["timed_out"], true);
            partial = true;
            break;
        }
    }
    assert!(
        partial,
        "non-reading raw PTY must eventually apply backpressure"
    );
    assert!(written > 0);
    assert_eq!(
        shell
            .call("ssmcp.exec.status", json!({"job_id":id}))
            .await
            .unwrap()["stdin"]["bytes_written"],
        written
    );
    assert!(
        shell
            .call(
                "ssmcp.exec.write",
                json!({"job_id":id,"data":"FORBIDDEN","close_stdin":true})
            )
            .await
            .is_err()
    );
    assert_eq!(
        shell
            .call("ssmcp.exec.status", json!({"job_id":id}))
            .await
            .unwrap()["stdin"]["bytes_written"],
        written
    );
    // Signal only this test's still-owned child after it installed its handler.
    assert_eq!(
        unsafe { libc::kill(started["pid"].as_u64().unwrap() as i32, libc::SIGUSR1) },
        0
    );
    until(
        &shell,
        id,
        &mut cursor,
        format!("INPUT:{written}:True").as_bytes(),
    )
    .await;
    assert_eq!(wait(&shell, id).await["exit_code"], 0);
    shell.shutdown().await;
}

#[tokio::test]
async fn cancelling_pty_wakes_blocked_writers_and_stops_owned_descendants() {
    let shell = Shell::new();
    let script = r#"import os,subprocess,tty,time
tty.setraw(0)
child=subprocess.Popen(['sleep','60'])
os.write(1,('CHILD:%d\n'%child.pid).encode())
time.sleep(60)
"#;
    let started = start(&shell, json!(["python3", "-c", script])).await;
    let id = started["job_id"].as_str().unwrap();
    let mut cursor = 0;
    let bytes = until(&shell, id, &mut cursor, b"\n").await;
    let line = String::from_utf8(bytes).unwrap();
    let child: i32 = line
        .split("CHILD:")
        .nth(1)
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    for _ in 0..8 {
        let receipt = shell
            .call(
                "ssmcp.exec.write",
                json!({"job_id":id,"data":"X".repeat(MAX_INPUT),"wait_ms":10}),
            )
            .await
            .unwrap();
        if receipt["timed_out"] == true {
            break;
        }
    }
    let input = shell.call(
        "ssmcp.exec.write",
        json!({"job_id":id,"data":"Y".repeat(MAX_INPUT),"wait_ms":5000}),
    );
    let queued = shell.call(
        "ssmcp.exec.write",
        json!({"job_id":id,"data":"Z".repeat(MAX_INPUT),"wait_ms":5000}),
    );
    let cancel = async {
        tokio::time::sleep(Duration::from_millis(30)).await;
        shell
            .call("ssmcp.exec.cancel", json!({"job_id":id}))
            .await
            .unwrap()
    };
    let (input, queued, cancel) = timeout(Duration::from_secs(4), async {
        tokio::join!(input, queued, cancel)
    })
    .await
    .unwrap();
    for result in [input.unwrap(), queued.unwrap()] {
        assert!(result["written"].as_u64().unwrap() <= MAX_INPUT as u64);
        assert!(!result["error"].is_null(), "{result}");
    }
    assert_eq!(cancel["running"], false);
    assert_eq!(cancel["cancelled"], true);
    timeout(Duration::from_secs(2), async {
        loop {
            let state = std::fs::read_to_string(format!("/proc/{child}/stat"))
                .ok()
                .and_then(|s| {
                    s.rsplit_once(')')
                        .map(|(_, tail)| tail.trim_start().chars().next().unwrap())
                });
            if matches!(state, None | Some('Z' | 'X')) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("owned descendant stopped");
    shell
        .call("ssmcp.exec.forget", json!({"job_id":id}))
        .await
        .unwrap();
    assert!(shell.registry().jobs.is_empty());
    shell.shutdown().await;
}

#[tokio::test]
async fn pty_timeout_validation_and_pipe_mode_preserve_execution_contracts() {
    let shell = Shell::new();
    for args in [
        json!({"mode":"pty","command":["touch","must-not-spawn"],"stdin":"pipe"}),
        json!({"mode":"pty","command":["touch","must-not-spawn"],"rows":0}),
        json!({"mode":"pipes","command":["touch","must-not-spawn"],"rows":24}),
        json!({"mode":"pty","command":["touch","must-not-spawn"],"temporary_artifacts":true}),
    ] {
        assert!(shell.call("ssmcp.exec.start", args).await.is_err());
        assert!(shell.registry().jobs.is_empty());
    }
    let started = shell
        .call(
            "ssmcp.exec.start",
            json!({"mode":"pty","command":["sleep","60"],"timeout_ms":100}),
        )
        .await
        .unwrap();
    let done = wait(&shell, started["job_id"].as_str().unwrap()).await;
    assert_eq!(done["timed_out"], true);
    assert_eq!(done["stdin"]["open"], false);
    let pipe=shell.call("ssmcp.exec.start",json!({"mode":"pipes","command":["python3","-c","import os; print([os.isatty(n) for n in (0,1,2)])"]})).await.unwrap();
    assert_eq!(pipe["mode"], "pipes");
    assert_eq!(pipe["output_mode"], "pipes");
    assert!(
        shell
            .call(
                "ssmcp.exec.resize",
                json!({"job_id":pipe["job_id"],"rows":20,"cols":80})
            )
            .await
            .is_err()
    );
    wait(&shell, pipe["job_id"].as_str().unwrap()).await;
    shell.shutdown().await;
}

#[tokio::test]
async fn pty_resize_sends_real_sigwinch_to_the_foreground_process() {
    let shell = Shell::new();
    let script = r#"import os,signal,termios,tty
tty.setraw(0)
def resized(*args):
    rows,cols=termios.tcgetwinsize(0)
    os.write(1,('WINCH:%d:%d\n'%(rows,cols)).encode())
signal.signal(signal.SIGWINCH,resized)
os.write(1,b'READY')
while True: signal.pause()
"#;
    let started = start(&shell, json!(["python3", "-c", script])).await;
    let id = started["job_id"].as_str().unwrap();
    let mut cursor = 0;
    until(&shell, id, &mut cursor, b"READY").await;
    let resized = shell
        .call(
            "ssmcp.exec.resize",
            json!({"job_id":id,"rows":41,"cols":137}),
        )
        .await
        .unwrap();
    assert_eq!(resized["rows"], 41);
    assert_eq!(resized["cols"], 137);
    until(&shell, id, &mut cursor, b"WINCH:41:137").await;
    let stopped = shell
        .call("ssmcp.exec.cancel", json!({"job_id":id}))
        .await
        .unwrap();
    assert_eq!(stopped["running"], false);
    shell.shutdown().await;
}

#[tokio::test]
async fn local_statistics_unify_pty_starts_without_counting_observations_or_main() {
    let shell = Shell::with_terminal(
        Path::new("/bin/sh"),
        std::env::current_dir().unwrap(),
        OutputStore::new_connection().unwrap(),
    )
    .unwrap();
    for _ in 0..3 {
        shell.call("terminal.list", json!({})).await.unwrap();
        shell.call("terminal.status", json!({})).await.unwrap();
        shell.call("ssmcp.exec.list", json!({})).await.unwrap();
        shell
            .call("ssmcp.exec.status", json!({"job_id":"main"}))
            .await
            .unwrap();
        let stats = shell.local_command_stats();
        assert_eq!((stats.calls, stats.spawned, stats.start_errors), (0, 0, 0));
        assert!(stats.jobs.is_empty());
    }

    let explicit = start(&shell, json!(["/bin/sh", "-c", "sleep 0.02; exit 7"])).await;
    let id = explicit["job_id"].as_str().unwrap();
    assert_eq!(wait(&shell, id).await["exit_code"], 7);
    let compatibility = shell
        .call("terminal.open", json!({"name":"statistics fixture"}))
        .await
        .unwrap();
    let alias = compatibility["job_id"].as_str().unwrap();
    assert_eq!(compatibility["terminal_id"], alias);
    shell
        .call(
            "terminal.write",
            json!({"terminal_id":alias,"data":"exit 0\n"}),
        )
        .await
        .unwrap();
    assert_eq!(wait(&shell, alias).await["exit_code"], 0);

    // Both validation paths reject before spawning and each counts once.
    assert!(
        shell
            .call(
                "ssmcp.exec.start",
                json!({"mode":"pty","command":["must-not-spawn"],"stdin":"pipe"})
            )
            .await
            .is_err()
    );
    assert!(
        shell
            .call("terminal.open", json!({"name":""}))
            .await
            .is_err()
    );

    // Explicit caller identity must survive the compatibility route, even
    // when call_scoped is entered outside that caller's task-local scope.
    let remote = crate::runtime::call_scope::CallScope::from_relay_for_client(
        91,
        Some("statistics-remote"),
        1,
        None,
        None,
    )
    .unwrap();
    let source = remote
        .scope(async { crate::runtime::call_scope::Source::capture() })
        .await;
    let hidden = shell
        .call_scoped(
            "terminal.open",
            json!({"name":"remote fixture"}),
            source.clone(),
        )
        .await
        .unwrap();
    let hidden_exec = shell
        .call_scoped(
            "ssmcp.exec.start",
            json!({"mode":"pty","command":["/bin/sh","-c","exit 0"]}),
            source,
        )
        .await
        .unwrap();
    wait(&shell, hidden_exec["job_id"].as_str().unwrap()).await;

    let before = shell.local_command_stats();
    let duration = before
        .jobs
        .iter()
        .find(|job| job.id == id)
        .unwrap()
        .duration_ms;
    assert!(duration >= 10);
    for _ in 0..3 {
        for job in [id, alias] {
            shell
                .call("ssmcp.exec.status", json!({"job_id":job}))
                .await
                .unwrap();
            shell
                .call("ssmcp.exec.read", json!({"job_id":job,"max_bytes":1024}))
                .await
                .unwrap();
            shell
                .call("ssmcp.exec.wait", json!({"job_id":job,"wait_ms":0}))
                .await
                .unwrap();
            shell
                .call("terminal.status", json!({"terminal_id":job}))
                .await
                .unwrap();
        }
        shell.call("terminal.list", json!({})).await.unwrap();
        shell.call("ssmcp.exec.list", json!({})).await.unwrap();
        let stats = shell.local_command_stats();
        assert_eq!((stats.calls, stats.spawned, stats.start_errors), (4, 2, 2));
        assert_eq!(stats.jobs.len(), 2);
        assert!(stats.jobs.iter().all(|job| job.id != "main"
            && job.id != hidden["job_id"].as_str().unwrap()
            && job.id != hidden_exec["job_id"].as_str().unwrap()));
        let failed = stats.jobs.iter().find(|job| job.id == id).unwrap();
        assert_eq!(failed.state, "failed");
        assert_eq!(failed.mode, "pty");
        assert_eq!(failed.duration_ms, duration);
        assert!(failed.usage.is_none(), "PTY has no wait4 accounting report");
        assert_eq!(
            stats.jobs.iter().find(|job| job.id == alias).unwrap().state,
            "completed"
        );
    }
    shell.shutdown().await;
}
