use super::*;

fn fixture(count: usize) -> Arc<Pool> {
    let directory = std::env::temp_dir().join(format!(
        "ssmcp-tty-unit-{}-{}",
        std::process::id(),
        NEXT_CALL.fetch_add(1, Ordering::Relaxed)
    ));
    crate::platform::private_directory(&directory).unwrap();
    let pool = Arc::new(Pool {
        terminal: PathBuf::from("/usr/bin/false"),
        helper: std::env::current_exe().unwrap(),
        directory,
        allocation: tokio::sync::Mutex::new(()),
        state: Mutex::new(State {
            slots: Vec::new(),
            active: HashMap::new(),
        }),
        closed: AtomicBool::new(false),
        tasks: Mutex::new(Vec::new()),
    });
    for _ in 0..count {
        pool.add_slot().unwrap().view.lock().unwrap().seen = Some(Instant::now());
    }
    pool
}
#[tokio::test]
async fn sequential_calls_reuse_slots_and_parallel_calls_stay_separate() {
    let pool = fixture(3);
    let cwd = std::env::current_dir().unwrap();
    let one = pool
        .begin("a".into(), &json!(["printf", "one"]), &cwd)
        .await
        .unwrap();
    let two = pool
        .begin("b".into(), &json!(["printf", "two"]), &cwd)
        .await
        .unwrap();
    assert_eq!(one.slot_id(), 1);
    assert_eq!(two.slot_id(), 2);
    one.append(b"first-output");
    two.append(b"second-output");
    one.finish("exit 0");
    let three = pool
        .begin("c".into(), &json!(["printf", "three"]), &cwd)
        .await
        .unwrap();
    assert_eq!(three.slot_id(), 1);
    let a = pool.read_view(json!({"slot":1,"cursor":0})).await.unwrap();
    let b = pool.read_view(json!({"slot":2,"cursor":0})).await.unwrap();
    let a =
        String::from_utf8(STANDARD.decode(a["data_base64"].as_str().unwrap()).unwrap()).unwrap();
    let b =
        String::from_utf8(STANDARD.decode(b["data_base64"].as_str().unwrap()).unwrap()).unwrap();
    assert!(a.contains("first-output"));
    assert!(!a.contains("second-output"));
    assert!(b.contains("second-output"));
    assert!(!b.contains("first-output"));
    drop(three);
    drop(two);
    assert!(pool.state.lock().unwrap().active.is_empty());
}
#[tokio::test]
async fn closed_interactive_terminals_release_stream_counters_and_reuse_the_slot() {
    let pool = fixture(1);
    let cwd = std::env::current_dir().unwrap();
    for index in 0..64 {
        let id = format!("pty-{index}");
        pool.terminal_begin(&id, &cwd).await.unwrap();
        pool.terminal_output(&id, b"output");
        assert!(
            pool.state
                .lock()
                .unwrap()
                .streamed
                .contains_key(&format!("terminal:{id}"))
        );
        pool.terminal_closed(&id);
        pool.terminal_closed(&id);
        pool.terminal_output(&id, b"late output");
    }
    let state = pool.state.lock().unwrap();
    assert!(state.active.is_empty());
    assert!(state.streamed.is_empty());
    assert_eq!(state.slots.len(), 1);
}

#[cfg(unix)]
#[tokio::test]
async fn failed_view_prevents_command_start_and_releases_slot() {
    let pool = fixture(1);
    let slot = pool.state.lock().unwrap().slots[0].clone();
    slot.view.lock().unwrap().seen = None;
    pool.launch(&slot).unwrap();
    let result = pool
        .begin("failed".into(), &json!(["must-not-run"]), Path::new("/tmp"))
        .await;
    assert!(result.is_err());
    assert!(pool.state.lock().unwrap().active.is_empty());
}
#[tokio::test]
async fn observer_tail_is_bounded_and_reports_lost_history() {
    let pool = fixture(1);
    let slot = pool.state.lock().unwrap().slots[0].clone();
    slot.append(&vec![b'a'; TAIL_BYTES + 20]);
    let response = pool.read_view(json!({"slot":1,"cursor":0})).await.unwrap();
    assert_eq!(response["truncated"], true);
    assert_eq!(response["next_cursor"], 32788);
    assert_eq!(slot.view.lock().unwrap().bytes.len(), TAIL_BYTES);
}
#[test]
fn launchers_keep_executable_arguments_literal() {
    let helper = Path::new("/tmp/with spaces/ssmcp");
    let directory = Path::new("/tmp/私有 endpoint");
    for (terminal, prefix) in [
        ("kitty", vec!["--title", "ssmcp · commands 7", "--"]),
        ("ztty", vec!["-e"]),
        ("wezterm", vec!["start", "--"]),
        ("gnome-terminal", vec!["--"]),
    ] {
        let args = launcher_args(Path::new(terminal), helper, directory, 7);
        let strings: Vec<_> = args.iter().map(|arg| arg.to_str().unwrap()).collect();
        assert_eq!(&strings[..prefix.len()], prefix.as_slice());
        assert_eq!(
            &strings[prefix.len()..],
            [
                "/tmp/with spaces/ssmcp",
                "__tty-view",
                "/tmp/私有 endpoint",
                "7"
            ]
        );
    }
    assert_eq!(
        display_command(&json!(["printf", "", "a'b", "空 格", "$(touch /tmp/no)"])),
        "printf '' 'a'\\''b' '空 格' '$(touch /tmp/no)'"
    );
    assert!(validate_terminal("").is_err());
    assert!(validate_terminal("kitty\nmalicious").is_err());
}
#[cfg(target_os = "linux")]
async fn shell(pool: &Arc<Pool>) -> Arc<crate::backend::shell::Shell> {
    let store = crate::output::OutputStore::new_connection().unwrap();
    store.set_observer(pool);
    crate::backend::shell::Shell::with_workspace(std::env::current_dir().unwrap(), store).unwrap()
}
#[cfg(target_os = "linux")]
#[tokio::test]
async fn managed_pty_preserves_argv_color_exit_and_pipe_stdin() {
    let pool = fixture(2);
    let shell = shell(&pool).await;
    let job=shell.call("ssmcp.exec.start",json!({
        "command":["/bin/sh","-c","test -t 1 && test -t 2 || exit 91; printf '\\033[32m%s\\033[0m\\n' \"$1\"; printf 'error-line\\n' >&2; cat; exit 7","sh","空 格 ' literal $(printf nope)"],
        "stdin":"pipe"
    })).await.unwrap();
    assert_eq!(job["output_mode"], "pty");
    assert_eq!(job["tty_slot"], 1);
    shell
        .call(
            "ssmcp.exec.write",
            json!({"job_id":job["job_id"],"data":"input exact\n","close_stdin":true}),
        )
        .await
        .unwrap();
    let done = shell
        .call(
            "ssmcp.exec.wait",
            json!({"job_id":job["job_id"],"wait_ms":5000}),
        )
        .await
        .unwrap();
    assert_eq!(done["exit_code"], 7);
    assert_eq!(done["running"], false);
    let read = shell
        .call("ssmcp.exec.read", json!({"job_id":job["job_id"]}))
        .await
        .unwrap();
    let text = read["stdout"].as_str().unwrap();
    assert!(text.contains("\x1b[32m空 格 ' literal $(printf nope)\x1b[0m"));
    assert!(text.contains("error-line"));
    assert!(text.contains("input exact"));
    assert_eq!(read["stderr"], "");
    assert!(pool.state.lock().unwrap().active.is_empty());
    shell.shutdown().await;
}
#[cfg(target_os = "linux")]
#[tokio::test]
async fn parallel_jobs_cancel_owned_children_and_reuse_observers() {
    let pool = fixture(3);
    let shell = shell(&pool).await;
    let a=shell.call("ssmcp.exec.start",json!({"command":["/bin/sh","-c","sleep 30 & p=$!; printf '%s\\n' \"$p\"; wait \"$p\""]})).await.unwrap();
    let b = shell
        .call(
            "ssmcp.exec.start",
            json!({"command":["/bin/sh","-c","sleep 1; printf completed"]}),
        )
        .await
        .unwrap();
    assert_ne!(a["tty_slot"], b["tty_slot"]);
    let read = shell
        .call(
            "ssmcp.exec.read",
            json!({"job_id":a["job_id"],"wait_ms":2000}),
        )
        .await
        .unwrap();
    let pid: i32 = read["stdout"].as_str().unwrap().trim().parse().unwrap();
    let cancelled = shell
        .call("ssmcp.exec.cancel", json!({"job_id":a["job_id"]}))
        .await
        .unwrap();
    assert_eq!(cancelled["running"], false);
    assert_eq!(cancelled["cancelled"], true);
    // SAFETY: signal 0 probes the test-owned descendant without sending a signal.
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
    let done = shell
        .call(
            "ssmcp.exec.wait",
            json!({"job_id":b["job_id"],"wait_ms":3000}),
        )
        .await
        .unwrap();
    assert_eq!(done["exit_code"], 0);
    let c = shell
        .call("ssmcp.exec.start", json!({"command":["/bin/true"]}))
        .await
        .unwrap();
    assert_eq!(c["tty_slot"], 1);
    shell
        .call(
            "ssmcp.exec.wait",
            json!({"job_id":c["job_id"],"wait_ms":3000}),
        )
        .await
        .unwrap();
    shell.shutdown().await;
}
#[cfg(target_os = "linux")]
#[tokio::test]
async fn failed_command_spawn_releases_observer_without_executing() {
    let pool = fixture(1);
    let shell = shell(&pool).await;
    assert!(
        shell
            .call(
                "ssmcp.exec.start",
                json!({"command":["/ssmcp-fixture-command-does-not-exist"]})
            )
            .await
            .is_err()
    );
    assert!(pool.state.lock().unwrap().active.is_empty());
    let jobs = shell.call("ssmcp.exec.list", json!({})).await.unwrap();
    assert!(jobs["jobs"].as_array().unwrap().is_empty());
    shell.shutdown().await;
}

#[test]
fn idle_extra_windows_retire_but_the_first_and_active_windows_remain() {
    let pool = fixture(3);
    {
        let state = pool.state.lock().unwrap();
        for slot in &state.slots {
            slot.view.lock().unwrap().last_used = Instant::now() - IDLE_WINDOW;
        }
    }
    pool.state.lock().unwrap().active.insert("busy".into(), 3);
    pool.retire_idle();
    let state = pool.state.lock().unwrap();
    assert!(!state.slots[0].view.lock().unwrap().retired);
    assert!(state.slots[1].view.lock().unwrap().retired);
    assert!(!state.slots[2].view.lock().unwrap().retired);
}

#[tokio::test]
async fn native_observer_leases_keep_concurrent_output_separate() {
    let pool = fixture(3);
    let cwd = std::env::current_dir().unwrap();
    let one = pool
        .begin("first".into(), &json!("first"), &cwd)
        .await
        .unwrap();
    let two = pool
        .begin("second".into(), &json!("second"), &cwd)
        .await
        .unwrap();
    one.append(b"alpha");
    two.append(b"beta");
    let output = String::from_utf8(
        one.slot
            .view
            .lock()
            .unwrap()
            .bytes
            .iter()
            .copied()
            .collect(),
    )
    .unwrap();
    assert!(output.contains("alpha"));
    assert!(!output.contains("beta"));
    one.finish("first complete");
    assert!(!pool.state.lock().unwrap().active.contains_key("first"));
    assert!(pool.state.lock().unwrap().active.contains_key("second"));
}
