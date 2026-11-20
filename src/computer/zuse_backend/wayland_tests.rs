//! Exercise the real worker protocol against owned IPC, without a live desktop.
use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const FIXTURE: &str = "computer::zuse_backend::wayland_tests::wayland_preflight_recovery_fixture";
const SIGNATURE: &str = "ssmcp-preflight-fixture";

#[tokio::test]
async fn wayland_preflight_refusals_preserve_same_host_connection() {
    let directory = CaptureDirectory::new().unwrap();
    let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", FIXTURE, "--ignored", "--nocapture"])
        .env("SSMCP_ZUSE_WAYLAND_PREFLIGHT_FIXTURE", "1")
        .env("XDG_RUNTIME_DIR", &directory.0)
        .env("HYPRLAND_INSTANCE_SIGNATURE", SIGNATURE)
        // An absolute, nonexistent display prevents fallback to the host seat.
        .env("WAYLAND_DISPLAY", directory.0.join("no-wayland-display"))
        .env_remove("DISPLAY")
        .env_remove("ZUSE_BACKEND")
        .kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(30), command.output())
        .await
        .expect("owned Wayland worker fixture timed out")
        .unwrap();
    assert!(
        output.status.success(),
        "owned Wayland worker fixture failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[derive(Default)]
struct State {
    caps: bool,
    num: bool,
    group: u8,
    held: bool,
    dispatched: usize,
}

async fn compositor(listener: tokio::net::UnixListener, state: Arc<StdMutex<State>>) {
    loop {
        let (mut stream, _) = listener.accept().await.unwrap();
        let request = tokio::time::timeout(Duration::from_secs(5), async {
            let mut bytes = Vec::new();
            loop {
                let mut buffer = [0u8; 512];
                let count = stream.read(&mut buffer).await.unwrap();
                assert_ne!(count, 0, "incomplete owned compositor request");
                bytes.extend_from_slice(&buffer[..count]);
                assert!(bytes.len() <= 4096, "unbounded compositor request");
                let request = std::str::from_utf8(&bytes).unwrap();
                if matches!(request, "j/version" | "j/devices")
                    || (request.starts_with("/repl return ") && request.ends_with("end)()"))
                    || (request.starts_with("/eval hl.dispatch(hl.dsp.send_shortcut(")
                        && request.ends_with("}))"))
                {
                    break request.to_owned();
                }
            }
        })
        .await
        .expect("owned compositor request was not completed");
        let response = {
            let mut state = state.lock().unwrap();
            match request.as_str() {
                "j/version" => json!({"tag":"v0.55.0"}).to_string(),
                "j/devices" => json!({"keyboards":[{
                    "main":true,"capsLock":state.caps,"numLock":state.num,
                    "active_layout_index":state.group
                }]})
                .to_string(),
                request if request.starts_with("/repl return ") => state.held.to_string(),
                request if request.starts_with("/eval hl.dispatch(hl.dsp.send_shortcut(") => {
                    state.dispatched += 1;
                    "ok".into()
                }
                _ => panic!("unexpected owned compositor request: {request}"),
            }
        };
        stream.write_all(response.as_bytes()).await.unwrap();
        // Closing this owned stream completes the native IPC response.
    }
}

#[tokio::test]
#[ignore = "spawned with a private environment by the Wayland worker regression"]
async fn wayland_preflight_recovery_fixture() {
    if std::env::var("SSMCP_ZUSE_WAYLAND_PREFLIGHT_FIXTURE").as_deref() != Ok("1") {
        return;
    }
    let root = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").unwrap());
    assert_eq!(
        PathBuf::from(std::env::var_os("WAYLAND_DISPLAY").unwrap()),
        root.join("no-wayland-display")
    );
    assert_eq!(
        std::env::var("HYPRLAND_INSTANCE_SIGNATURE").unwrap(),
        SIGNATURE
    );
    let socket = root.join("hypr").join(SIGNATURE).join(".socket.sock");
    std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let state = Arc::new(StdMutex::new(State::default()));
    let server = tokio::spawn(compositor(listener, state.clone()));
    let computer = Computer::new(root).unwrap();
    assert_eq!(computer.session, Session::Hyprland);
    computer.handoff_host_input_local(60).unwrap();

    let cases = [
        (
            true,
            false,
            0,
            false,
            "unsupported_state: Caps Lock is enabled; disable it before input",
        ),
        (
            false,
            true,
            0,
            false,
            "unsupported_state: Num Lock is enabled; disable it before input",
        ),
        (
            false,
            false,
            1,
            false,
            "unsupported_state: keyboard layout group must be 0 before input",
        ),
        (
            false,
            false,
            0,
            true,
            "unsupported_state: a keyboard key is held; release it before input",
        ),
    ];
    for (caps, num, group, held, expected) in cases {
        let before = {
            let mut state = state.lock().unwrap();
            state.caps = caps;
            state.num = num;
            state.group = group;
            state.held = held;
            state.dispatched
        };
        let error = computer
            .perform_host("ssmcp.computer.key", json!({"key":"a"}))
            .await
            .unwrap_err();
        assert_eq!(error, expected);
        assert_eq!(state.lock().unwrap().dispatched, before);
        assert!(!HOST_INPUT_CLEANUP_FAILED.load(Ordering::Acquire));

        {
            let mut state = state.lock().unwrap();
            state.caps = false;
            state.num = false;
            state.group = 0;
            state.held = false;
        }
        computer
            .perform_host("ssmcp.computer.key", json!({"key":"a"}))
            .await
            .unwrap();
        assert_eq!(state.lock().unwrap().dispatched, before + 1);
    }

    for (text, expected) in [
        (
            "A\u{1}B",
            "unsupported control U+0001 at byte 1; text permits only Tab, LF and CR",
        ),
        (
            "🙂\u{7f}B",
            "unsupported control U+007F at byte 4; text permits only Tab, LF and CR",
        ),
    ] {
        let before = state.lock().unwrap().dispatched;
        let error = computer
            .perform_host("ssmcp.computer.type", json!({"text":text}))
            .await
            .unwrap_err();
        // Without whole-text preflight, the first scalar instead tries to
        // connect to the deliberately absent native Wayland keyboard display.
        assert_eq!(error, expected);
        assert_eq!(state.lock().unwrap().dispatched, before);
        assert!(!HOST_INPUT_CLEANUP_FAILED.load(Ordering::Acquire));
        computer
            .perform_host("ssmcp.computer.key", json!({"key":"a"}))
            .await
            .unwrap();
        assert_eq!(state.lock().unwrap().dispatched, before + 1);
    }
    computer.shutdown().await.unwrap();
    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
}

#[test]
fn uncertain_wayland_errors_are_not_preflight_refusals() {
    for error in [
        "Wayland input exceeded its deadline",
        "Wayland write failed",
        "Wayland display disconnected",
        "input completed but keyboard keymap restoration failed: Wayland write failed",
        "unsupported_state: Caps Lock is enabled; disable it before input; keyboard keymap restoration failed: Wayland write failed",
        "unsupported_state: unrecognized keyboard state",
    ] {
        for typing in [false, true] {
            assert!(
                !input_error_precedes_effect("wayland", error, typing),
                "{error}"
            );
        }
    }
}

#[test]
fn text_preflight_preserves_unicode_and_other_backend_controls() {
    validate_literal_text("wayland", "A中文🙂\t\n\r\u{200d}\u{301}").unwrap();
    for backend in ["x11", "win32", "quartz"] {
        validate_literal_text(backend, "A\u{1}\u{7f}B").unwrap();
    }
}
