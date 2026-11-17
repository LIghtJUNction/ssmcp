//! Default reconnect policy against an isolated failing SSH executable.
#![cfg(target_os = "linux")]

use std::{
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::Stdio,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{process::Command, time::timeout};

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "ssmcp-watchdog-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&path).unwrap();
        let ssh = path.join("ssh");
        std::fs::write(&ssh, "#!/usr/bin/python3\nimport os, pathlib, sys\nwith pathlib.Path(os.environ['SSMCP_WATCHDOG_ATTEMPTS']).open('a') as f:\n f.write('attempt\\n')\nsys.exit(255)\n").unwrap();
        std::fs::set_permissions(ssh, std::fs::Permissions::from_mode(0o700)).unwrap();
        Self(path)
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ssmcp"));
        command
            .args(["--no-setup", "--shell", "/bin/sh"])
            .env("PATH", &self.0)
            .env("SSMCP_WATCHDOG_ATTEMPTS", self.0.join("attempts"))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        command
    }

    fn attempts(&self) -> usize {
        std::fs::read_to_string(self.0.join("attempts"))
            .unwrap_or_default()
            .lines()
            .count()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::test]
async fn default_watchdog_retries_and_ctrl_c_stops_it() {
    let fixture = Fixture::new();
    let child = fixture
        .command()
        .args(["--reconnect-delay", "1", "fixture"])
        .spawn()
        .unwrap();
    timeout(Duration::from_secs(6), async {
        while fixture.attempts() < 2 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("default watchdog did not retry a failed connection");
    // Signal only this owned test process, as an interactive Ctrl-C would.
    assert_eq!(
        unsafe { libc::kill(child.id().unwrap() as i32, libc::SIGINT) },
        0
    );
    let output = timeout(Duration::from_secs(3), child.wait_with_output())
        .await
        .expect("Ctrl-C did not stop the default watchdog")
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("retrying"));
}

#[tokio::test]
async fn no_watchdog_returns_the_first_connection_failure() {
    let fixture = Fixture::new();
    let output = timeout(
        Duration::from_secs(3),
        fixture
            .command()
            .args(["--no-watchdog", "fixture"])
            .output(),
    )
    .await
    .expect("--no-watchdog did not exit after connection failure")
    .unwrap();
    assert!(!output.status.success());
    assert_eq!(fixture.attempts(), 1);
}
