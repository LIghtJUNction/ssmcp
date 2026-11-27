//! Actual CLI checks use private loopback peers; no tool backend or account is created.
#![cfg(unix)]
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const TOKEN: &str = "cl_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
struct Home(PathBuf);
impl Home {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "ssmcp-allow-account-{}-{timestamp}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        fs::create_dir(path.join("auth")).unwrap();
        fs::set_permissions(path.join("auth"), fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(path.join("config.toml"), "[ui]\ncolor = false\n").unwrap();
        Self(path)
    }
    fn account(&self, server: &str) {
        let path = self.0.join("auth").join(format!(
            "{}.json",
            Sha256::digest(server.as_bytes())
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        ));
        let expires = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 3600;
        fs::write(&path,serde_json::to_vec(&json!({"server":server,"token":TOKEN,"principal":"owner@github","subject":"123","username":"owner-home","expires_at":expires})).unwrap()).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    async fn run(&self, args: &[&str]) -> std::process::Output {
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_ssmcp"));
        command
            .args(args)
            .env("HOME", &self.0)
            .env("XDG_CONFIG_HOME", self.0.join("config"))
            .env("XDG_STATE_HOME", self.0.join("state"))
            .env("SSMCP_SESSION_HOME", self.0.join("sessions"))
            .env("SSMCP_AUTH_HOME", self.0.join("auth"))
            .env("SSMCP_CLIENT_CONFIG", self.0.join("config.toml"))
            .env_remove("SSMCP_UI_CONFIG")
            .env("SSMCP_RELAY_TOKEN", TOKEN)
            .stdin(std::process::Stdio::null())
            .env("NO_COLOR", "1")
            .kill_on_drop(true);
        tokio::time::timeout(Duration::from_secs(10), command.output())
            .await
            .unwrap()
            .unwrap()
    }
}
impl Drop for Home {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[tokio::test]
async fn device_login_help_lists_only_current_commands() {
    let home = Home::new();
    for args in [
        vec!["--help"],
        vec!["--help-full"],
        vec!["sessions", "create", "--help"],
        vec!["login", "--help"],
        vec!["join", "--help"],
    ] {
        let output = home.run(&args).await;
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stderr.is_empty());
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(!text.contains("--allow"), "{args:?}: {text}");
        match args.as_slice() {
            ["login", "--help"] => assert!(text.contains("ssmcp login USERNAME@SERVER")),
            ["join", "--help"] => {
                assert!(text.contains("ssmcp join USERNAME@SERVER --token-stdin"))
            }
            ["--help-full"] => {
                assert!(text.contains("ssmcp login USERNAME@SERVER"));
                assert!(text.contains("ssmcp join USERNAME@SERVER --token-stdin"));
            }
            _ => {}
        }
    }
    let removed = home.run(&["invite", "--help"]).await;
    assert!(!removed.status.success());
    assert!(String::from_utf8_lossy(&removed.stderr).contains("invite subcommand was removed"));
}

#[tokio::test]
async fn noninteractive_device_never_uses_a_controller_token_or_unsigned_transport() {
    let home = Home::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    for scheme in ["http", "https"] {
        let base = format!("{scheme}://{address}");
        home.account(&base);
        let target = format!("alice@{base}");
        let output = home.run(&[&target, "--no-watchdog"]).await;
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(if scheme == "https" {
                "ssmcp login"
            } else {
                "HTTPS"
            }),
            "{stderr}"
        );
        assert!(!stderr.contains(TOKEN));
        assert!(!stderr.contains("Starting native tools"));
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(50), listener.accept())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn removed_allow_flags_and_implicit_targets_cannot_open_a_session() {
    let home = Home::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("https://{}", listener.local_addr().unwrap());
    let target = format!("alice@{base}");
    for args in [
        vec![target.as_str(), "--allow", "bob"],
        vec![target.as_str(), "--allow=bob"],
        vec!["--server", &base, "--allow", "bob@another.example"],
    ] {
        let output = home.run(&args).await;
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(
            error.contains("--allow was removed") && error.contains("/allow add"),
            "{error}"
        );
    }
    home.account(&base);
    let path = home.0.join("auth/last-connection.json");
    let target = json!({"mode":"relay", "username":"saved-target", "server":base});
    fs::write(&path, serde_json::to_vec(&target).unwrap()).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    let output = home.run(&["--server", &base, "--no-watchdog"]).await;
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("ssmcp username@server"));
    assert!(
        tokio::time::timeout(Duration::from_millis(50), listener.accept())
            .await
            .is_err()
    );
}
