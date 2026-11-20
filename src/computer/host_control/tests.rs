use super::*;
use std::{
    io::Write,
    path::Path,
    process::{Child, Command, Stdio},
    sync::{Barrier, Mutex},
    time::Instant,
};

fn peer(control: &HostControl) -> HostControl {
    HostControl::at(control.store().unwrap().path().to_owned())
}

#[test]
fn a_fresh_store_starts_on_and_explicit_off_persists_across_connections() {
    let control = HostControl::default();
    assert_eq!(control.status()["enabled"], true);
    assert_eq!(control.status()["background_enabled"], true);
    assert!(control.ticket().is_ok());
    assert!(control.store().unwrap().path().exists());
    let enabled = control.set_local(true).unwrap();
    assert_eq!(enabled["enabled"], true);
    assert_eq!(enabled["scope"], "machine_os_user");
    assert_eq!(enabled, peer(&control).status());
    assert_eq!(control.set_local(true).unwrap(), enabled);
    assert_eq!(control.set_local(false).unwrap()["enabled"], false);
    assert_eq!(peer(&control).status()["enabled"], false);
    assert_eq!(control.ticket().unwrap_err(), LOCKED);
}

#[test]
fn missing_state_during_a_live_session_does_not_restore_default_on() {
    let control = HostControl::default();
    let original = control.ticket().unwrap();
    let root = control.store().unwrap().path();
    std::fs::remove_file(root.join("state.json")).unwrap();
    assert_eq!(control.status()["enabled"], false);
    assert!(!control.valid(original));
    assert_eq!(peer(&control).status()["enabled"], false);
    assert!(!root.join("state.json").exists());
    std::fs::remove_dir(root).unwrap();
    assert_eq!(control.status()["enabled"], false);
    assert!(control.ticket().is_err());
    assert!(
        !root.exists(),
        "a read must never initialize another on state"
    );
}

#[test]
fn existing_directory_without_state_is_not_treated_as_a_fresh_installation() {
    let control = HostControl::default();
    let root = control.store().unwrap().path();
    std::fs::remove_file(root.join("state.json")).unwrap();
    let reopened = Store::at(root.to_owned());
    reopened.initialize().unwrap();
    assert_eq!(reopened.read().unwrap(), State::default());
    assert!(!root.join("state.json").exists());
}

#[test]
fn restarting_initialization_preserves_explicit_off_and_invalid_state() {
    let control = HostControl::default();
    let root = control.store().unwrap().path();
    control.set_local(false).unwrap();
    let reopened = Store::at(root.to_owned());
    reopened.initialize().unwrap();
    assert!(!reopened.read().unwrap().enabled);
    std::fs::write(root.join("state.json"), b"{invalid").unwrap();
    reopened.initialize().unwrap();
    assert!(reopened.read().is_err());
    assert_eq!(control.status()["enabled"], false);
}

#[test]
fn explicit_on_has_no_timer_and_new_connections_share_the_setting() {
    let control = HostControl::default();
    control.set_local(true).unwrap();
    let ticket = control.ticket().unwrap();
    let state = control.store().unwrap().read().unwrap();
    // An old persisted record is still an explicit on, with no clock-based
    // deadline. Reopening is a new control instance, not a paused Tokio clock.
    let path = control.store().unwrap().path().join("state.json");
    let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    file.set_times(
        std::fs::FileTimes::new().set_modified(std::time::UNIX_EPOCH + Duration::from_secs(1)),
    )
    .unwrap();
    let reopened = Store::at(control.store().unwrap().path().to_owned());
    reopened.initialize().unwrap();
    assert_eq!(reopened.read().unwrap(), state);
    let other = peer(&control);
    assert!(control.valid(ticket));
    assert_eq!(other.status()["enabled"], true);
    assert_eq!(other.ticket().unwrap().generation, ticket.generation);
    assert!(other.status().get("remaining_seconds").is_none());
    assert!(other.status().get("expires_at").is_none());
}

#[test]
fn off_and_fresh_on_never_rearm_old_tickets() {
    let control = HostControl::default();
    let other = peer(&control);
    control.set_local(true).unwrap();
    let old = control.ticket().unwrap();
    other.set_local(false).unwrap();
    assert!(!control.valid(old));
    other.set_local(true).unwrap();
    assert!(!control.valid(old));
    let current = control.ticket().unwrap();
    assert!(control.valid(current));
    assert_ne!(current.generation, old.generation);
}

#[tokio::test]
async fn closing_one_connection_keeps_other_connections_enabled() {
    let control = HostControl::default();
    let other = peer(&control);
    control.set_local(true).unwrap();
    let first = control.ticket().unwrap();
    let second = other.ticket().unwrap();
    control.close();
    assert!(!control.valid(first));
    assert!(other.valid(second));
    assert_eq!(control.status()["enabled"], true);
    assert!(control.set_local(true).is_err());
    assert!(control.toggle_local().is_err());
    assert!(control.ticket().is_err());
    assert_eq!(
        control.run(first, async { Ok(()) }).await,
        Err(ENDED.into())
    );
    // Explicit local off remains possible even during connection shutdown.
    control.set_local(false).unwrap();
    assert_eq!(other.status()["enabled"], false);
}

#[test]
fn each_pointer_keyboard_and_focus_operation_requires_the_switch() {
    for suffix in ["focus", "move", "click", "drag", "key", "type", "scroll"] {
        assert!(input_tool(&format!("ssmcp.computer.{suffix}")));
    }
    for suffix in [
        "status",
        "windows",
        "screenshot",
        "desktop_start",
        "desktop_status",
        "desktop_stop",
        "app_launch",
    ] {
        assert!(!input_tool(&format!("ssmcp.computer.{suffix}")));
    }
}

#[tokio::test]
async fn off_cancels_queued_input_without_starting_the_operation() {
    let control = HostControl::default();
    let other = peer(&control);
    control.set_local(true).unwrap();
    let ticket = control.ticket().unwrap();
    let held = control.acquire(ticket).await.unwrap();
    let queued = other.acquire(ticket);
    tokio::pin!(queued);
    assert!(
        tokio::time::timeout(Duration::from_millis(40), &mut queued)
            .await
            .is_err()
    );
    control.set_local(false).unwrap();
    control.set_local(true).unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), queued)
            .await
            .unwrap()
            .err()
            .unwrap(),
        ENDED
    );
    drop(held);
    assert!(other.acquire(other.ticket().unwrap()).await.is_ok());
}

#[tokio::test]
async fn off_drops_running_input_before_its_guard_is_released() {
    struct MarkDropped<'a>(&'a AtomicBool);
    impl Drop for MarkDropped<'_> {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }
    let control = HostControl::default();
    let other = peer(&control);
    control.set_local(true).unwrap();
    let ticket = control.ticket().unwrap();
    let lease = control.acquire(ticket).await.unwrap();
    let dropped = AtomicBool::new(false);
    let operation = async {
        let _guard = MarkDropped(&dropped);
        std::future::pending::<Result<(), String>>().await
    };
    let running = control.run(ticket, operation);
    tokio::pin!(running);
    assert!(
        tokio::time::timeout(Duration::from_millis(40), &mut running)
            .await
            .is_err()
    );
    other.set_local(false).unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), running)
            .await
            .unwrap(),
        Err(ENDED.into())
    );
    assert!(dropped.load(Ordering::Acquire));
    // A release guard can retain the lease after cancelling platform work.
    assert!(other.store().unwrap().try_input().unwrap().is_none());
    drop(lease);
    assert!(other.store().unwrap().try_input().unwrap().is_some());
}

#[test]
fn simultaneous_toggles_are_serialized_without_lost_updates() {
    let control = HostControl::default();
    control.set_local(false).unwrap();
    let barrier = Arc::new(Barrier::new(4));
    let generations = Arc::new(Mutex::new(Vec::new()));
    let workers = (0..4)
        .map(|_| {
            let other = peer(&control);
            let barrier = barrier.clone();
            let generations = generations.clone();
            std::thread::spawn(move || {
                barrier.wait();
                for _ in 0..4 {
                    let status = other.toggle_local().unwrap();
                    generations
                        .lock()
                        .unwrap()
                        .push(status["generation"].as_str().unwrap().to_owned());
                }
            })
        })
        .collect::<Vec<_>>();
    for worker in workers {
        worker.join().unwrap();
    }
    assert_eq!(control.status()["enabled"], false);
    let mut generations = generations.lock().unwrap().clone();
    generations.sort();
    generations.dedup();
    assert_eq!(generations.len(), 16);
}

#[test]
fn state_corruption_is_off_and_only_explicit_off_repairs_it() {
    let control = HostControl::default();
    control.set_local(true).unwrap();
    let path = control.store().unwrap().path().join("state.json");
    for bytes in [
        b"{\"enabled\":true".as_slice(),
        br#"{"schema_version":2,"enabled":true,"generation":[1]}"#.as_slice(),
        br#"{"schema_version":1,"enabled":false,"generation":[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],"remote_grant":true}"#.as_slice(),
    ] {
        std::fs::write(&path, bytes).unwrap();
        assert_eq!(control.status()["enabled"], false);
        assert!(control.status()["error"].is_string());
        assert!(control.ticket().is_err());
        assert!(control.toggle_local().is_err());
        assert!(control.set_local(true).is_err());
        control.set_local(false).unwrap();
        assert!(control.status().get("error").is_none());
    }
    std::fs::write(&path, vec![b'x'; 1025]).unwrap();
    assert!(control.ticket().is_err());
    control.set_local(false).unwrap();
    assert_eq!(control.status()["enabled"], false);
}

#[cfg(unix)]
#[test]
fn shared_state_rejects_links_and_non_private_storage() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let control = HostControl::default();
    control.set_local(false).unwrap();
    let root = control.store().unwrap().path();
    let state = root.join("state.json");
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(control.set_local(true).is_err());
    assert!(control.status()["error"].is_string());
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::hard_link(&state, root.join("alias")).unwrap();
    assert!(control.set_local(true).is_err());
    std::fs::remove_file(root.join("alias")).unwrap();
    std::fs::rename(&state, root.join("original")).unwrap();
    symlink("original", &state).unwrap();
    assert!(control.set_local(true).is_err());
    std::fs::remove_file(&state).unwrap();
    std::fs::rename(root.join("original"), &state).unwrap();
    std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(control.set_local(true).is_err());
    std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700)).unwrap();
    control.set_local(true).unwrap();
    assert_eq!(control.status()["enabled"], true);
}

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

async fn child_marker(child: &mut Child, path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !path.exists() {
        assert!(child.try_wait().unwrap().is_none(), "fixture exited early");
        assert!(Instant::now() < deadline, "fixture marker timed out");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// This child runs only against the parent's explicit private temporary store.
#[test]
#[ignore = "subprocess fixture; called by shared_switch_crosses_process_boundaries"]
fn host_control_process_fixture() {
    use std::io::Read;
    let root = std::env::var_os("SSMCP_HOST_INPUT_TEST_ROOT").expect("private test root");
    let root = std::path::PathBuf::from(root);
    assert!(
        root.file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("ssmcp-host-input-test-")
    );
    let control = HostControl::at(root.clone());
    assert_eq!(control.status()["enabled"], true);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let ticket = control.ticket().unwrap();
        let lease = control.acquire(ticket).await.unwrap();
        std::fs::write(root.join("ready"), b"locked").unwrap();
        assert_eq!(
            control
                .run(ticket, std::future::pending::<Result<(), String>>())
                .await,
            Err(ENDED.into())
        );
        // Model delayed key-release cleanup while the cross-process lock lives.
        std::fs::write(root.join("cancelled"), b"releasing").unwrap();
        std::io::stdin().read_exact(&mut [0u8]).unwrap();
        drop(lease);
    });
}

#[tokio::test]
async fn shared_switch_crosses_process_boundaries() {
    let control = HostControl::default();
    let other = peer(&control);
    let root = control.store().unwrap().path();
    control.set_local(true).unwrap();
    let original = control.ticket().unwrap();
    let mut child = ChildGuard(
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "computer::host_control::tests::host_control_process_fixture",
                "--ignored",
                "--nocapture",
            ])
            .env("SSMCP_HOST_INPUT_TEST_ROOT", root)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    child_marker(&mut child.0, &root.join("ready")).await;
    assert!(other.store().unwrap().try_input().unwrap().is_none());
    control.set_local(false).unwrap();
    child_marker(&mut child.0, &root.join("cancelled")).await;
    assert!(other.store().unwrap().try_input().unwrap().is_none());
    control.set_local(true).unwrap();
    assert!(!other.valid(original));
    assert!(other.store().unwrap().try_input().unwrap().is_none());
    child.0.stdin.take().unwrap().write_all(b"x").unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "fixture did not release input");
        tokio::time::sleep(Duration::from_millis(5)).await;
    };
    assert!(status.success());
    assert!(other.store().unwrap().try_input().unwrap().is_some());
    assert_eq!(other.status()["enabled"], true);
}
