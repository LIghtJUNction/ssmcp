//! Exercise the public CLI with isolated configuration and account storage.
#![cfg(unix)]
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

struct Home(PathBuf);
impl Home {
    fn new() -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "ssmcp-client-config-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(path.join("auth")).unwrap();
        fs::set_permissions(path.join("auth"), fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(
            path.join("config.toml"),
            "# user theme\n[ui]\ntheme = 'nord'\ncolor = false\n",
        )
        .unwrap();
        Self(path)
    }
    fn command(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_ssmcp"))
            .args(args)
            .env("SSMCP_CLIENT_CONFIG", self.0.join("config.toml"))
            .env("SSMCP_AUTH_HOME", self.0.join("auth"))
            .env("SSMCP_SESSION_HOME", self.0.join("sessions"))
            .env_remove("SSMCP_CONTEXT")
            .env_remove("SSMCP_UI_CONFIG")
            .env_remove("SSMCP_RELAY_TOKEN")
            .env("NO_COLOR", "1")
            .output()
            .unwrap()
    }
    fn success(&self, args: &[&str]) -> String {
        let output = self.command(args);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }
    fn remember_target(&self, server: &str, username: &str) {
        write_private(
            &self.0.join("auth/last-connection.json"),
            &serde_json::to_vec(&json!({"mode": "relay", "server": server, "username": username}))
                .unwrap(),
        );
    }
    fn account(&self, server: &str, token: &str) {
        let expires = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 3600;
        let path = self.0.join("auth").join(format!(
            "{}.json",
            Sha256::digest(server.as_bytes())
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        ));
        write_private(
            &path,
            &serde_json::to_vec(&json!({
                "server": server, "token": token, "principal": "alice@github",
                "subject": "123", "expires_at": expires
            }))
            .unwrap(),
        );
        write_private(&self.0.join("auth/default"), server.as_bytes());
    }
}
impl Drop for Home {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn unix_default_shell_falls_back_when_shell_is_unavailable() {
    let home = Home::new();
    let missing = home.0.join("missing/zsh");
    let non_executable = home.0.join("zsh");
    write_private(&non_executable, b"#!/bin/sh\nexit 0\n");
    let mut client = Command::new(env!("CARGO_BIN_EXE_ssmcp"));
    client
        .arg("--list-tools")
        .current_dir(&home.0)
        .env_clear()
        .env("HOME", &home.0)
        .env("SSMCP_CLIENT_CONFIG", home.0.join("config.toml"))
        .env("PATH", "");

    for shell in [&missing, &non_executable] {
        let output = client.env("SHELL", shell).output().unwrap();
        assert!(
            output.status.success(),
            "unavailable SHELL {shell:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let catalog: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert!(
            catalog["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| tool["name"] == "terminal.write")
        );
    }

    let output = client.arg("--shell").arg(&missing).output().unwrap();
    assert!(
        !output.status.success(),
        "an explicit missing shell must fail"
    );
}

#[test]
fn zrun_launch_failure_logs_both_fallbacks_and_the_explicit_error() {
    let home = Home::new();
    let zrun = home.0.join("zrun");
    let system = home.0.join("system-shell");
    for path in [&zrun, &system] {
        fs::write(path, b"#!/no-such-ssmcp-shell-interpreter\n").unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let mut client = Command::new(env!("CARGO_BIN_EXE_ssmcp"));
    client
        .args(["--list-tools", "--log-format", "jsonl", "--shell", "zrun"])
        .current_dir(&home.0)
        .env_clear()
        .env("HOME", &home.0)
        .env("SSMCP_CLIENT_CONFIG", home.0.join("config.toml"))
        .env("SHELL", &system)
        .env("PATH", &home.0);
    let output = client.output().unwrap();
    let logs = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{logs}");
    assert!(
        logs.contains(r#""level":"Warning""#) && logs.contains("Trying"),
        "{logs}"
    );
    assert!(
        logs.contains("system-shell") && logs.contains("/bin/sh"),
        "{logs}"
    );

    let output = client.arg("--shell").arg(&system).output().unwrap();
    let logs = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{logs}");
    assert!(logs.contains("No usable SSMCP shell"), "{logs}");
}

fn write_private(path: &Path, bytes: &[u8]) {
    fs::write(path, bytes).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}

struct Server {
    origin: String,
    task: thread::JoinHandle<String>,
}
impl Server {
    fn once(status: &str, body: serde_json::Value) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let status = status.to_owned();
        let task = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut socket = loop {
                match listener.accept() {
                    Ok((socket, _)) => break socket,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            Instant::now() < deadline,
                            "CLI did not reach selected server"
                        );
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("{error}"),
                }
            };
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut bytes = Vec::new();
            let mut buffer = [0u8; 4096];
            loop {
                let n = socket.read(&mut buffer).unwrap();
                assert_ne!(n, 0, "request ended prematurely");
                bytes.extend_from_slice(&buffer[..n]);
                if let Some(index) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&bytes[..index]).to_lowercase();
                    let length = headers
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length:"))
                        .map(|value| value.trim().parse::<usize>().unwrap())
                        .unwrap_or(0);
                    if bytes.len() >= index + 4 + length {
                        break;
                    }
                }
                assert!(bytes.len() < 64 * 1024);
            }
            let body = body.to_string();
            write!(socket, "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            String::from_utf8(bytes).unwrap()
        });
        Self { origin, task }
    }
    fn request(self) -> String {
        self.task.join().unwrap()
    }
}

#[test]
fn server_configuration_is_editable_and_precedence_is_consistent() {
    let home = Home::new();
    assert_eq!(
        home.success(&["config", "get", "server"]).trim(),
        "https://ssmcp.lmm.best"
    );
    home.account("https://previous.example", &"a".repeat(64));
    assert_eq!(
        home.success(&["config", "get", "server"]).trim(),
        "https://previous.example"
    );
    home.success(&["config", "set", "server", "https://RELAY.example:443/"]);
    assert_eq!(
        home.success(&["config", "get", "server"]).trim(),
        "https://relay.example"
    );
    let text = fs::read_to_string(home.0.join("config.toml")).unwrap();
    assert!(text.contains("# user theme"));
    assert!(text.contains("theme = 'nord'"));
    // A plain edit is the same setting as the CLI command.
    fs::write(
        home.0.join("config.toml"),
        text.replace("https://relay.example", "https://edited.example"),
    )
    .unwrap();
    assert_eq!(
        home.success(&["config", "get", "server"]).trim(),
        "https://edited.example"
    );
    assert_eq!(
        Path::new(home.success(&["config", "path"]).trim()),
        home.0.join("config.toml")
    );
    // No credential exists for edited.example: never reuse the last-login token.
    let denied = home.command(&["whoami"]);
    assert!(!denied.status.success());
    assert!(String::from_utf8_lossy(&denied.stderr).contains("choose an account"));
    let connection = home.command(&["--no-watchdog", "--name", "config-check"]);
    assert!(!connection.status.success());
    assert!(connection.stdout.is_empty());
    let error = String::from_utf8_lossy(&connection.stderr);
    assert!(error.contains("ssmcp username@server"), "{error}");
    assert!(!error.contains(&"a".repeat(64)));
    home.success(&["config", "unset", "server"]);
    assert_eq!(
        home.success(&["config", "get", "server"]).trim(),
        "https://previous.example"
    );
}

#[test]
fn explicit_server_overrides_configuration_for_a_public_account_lookup() {
    let home = Home::new();
    home.success(&["config", "set", "server", "https://configured.example"]);
    let server = Server::once("200 OK", json!({"username":"alice-home"}));
    let token = "c".repeat(64);
    home.account(&server.origin, &token);
    home.remember_target(&server.origin, "alice-home");
    let output = home.success(&["--server", &server.origin, "whoami", "--json"]);
    let account: serde_json::Value = serde_json::from_str(&output).unwrap();
    assert_eq!(account["server"], server.origin);
    assert_eq!(account["account"]["username"], "alice-home");
    assert!(!output.contains("ssmcp.lmm.best"));
    assert!(!output.contains("configured.example"));
    assert!(!output.contains(&token));
    let request = server.request();
    assert!(request.starts_with("GET /api/client/account/alice-home "));
    assert!(!request.to_ascii_lowercase().contains("authorization:"));
    assert!(!request.contains(&token));

    // Device startup requires HTTPS even when a public lookup accepts loopback HTTP.
    let relay = "http://127.0.0.1:1";
    home.account(relay, &token);
    let allow = format!("alice@{relay}");
    let output = home.command(&[
        "--server",
        relay,
        &allow,
        "--no-watchdog",
        "--no-browser",
        "--name",
        "config-override-fixture",
    ]);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(
        error.contains("device authorization requires a named account on an HTTPS origin"),
        "{error}"
    );
    assert!(!error.contains("Starting native tools"), "{error}");
    assert!(!error.contains(&token));
    // Per-command selection leaves the saved configuration untouched.
    assert_eq!(
        home.success(&["config", "get", "server"]).trim(),
        "https://configured.example"
    );
}

#[test]
fn public_account_lookup_does_not_use_saved_login() {
    let home = Home::new();
    let server = Server::once("200 OK", json!({"username":"alice-home"}));
    let selected_token = "d".repeat(64);
    let previous_token = "e".repeat(64);
    home.account(&server.origin, &selected_token);
    home.account("https://last-login.example", &previous_token);
    home.success(&["config", "set", "server", &server.origin]);
    home.remember_target(&server.origin, "alice-home");
    let output = home.success(&["whoami", "--json"]);
    let account: serde_json::Value = serde_json::from_str(&output).unwrap();
    assert_eq!(account["server"], server.origin);
    assert!(!output.contains(&selected_token));
    assert!(!output.contains(&previous_token));
    let request = server.request();
    assert!(request.starts_with("GET /api/client/account/alice-home "));
    assert!(!request.to_ascii_lowercase().contains("authorization:"));
    assert!(!request.contains(&selected_token));
    assert!(!request.contains(&previous_token));

    let output = home.command(&["Alice@relay.example", "--register", "--no-browser"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("relay web portal"));
}

#[test]
fn selected_server_credentials_and_redirects_never_leak_to_another_origin() {
    let home = Home::new();
    let first = Server::once(
        "200 OK",
        json!({
            "identity": {"provider":"github", "subject":"123", "login":"alice", "principal":"alice@github"}
        }),
    );
    let first_token = "1".repeat(64);
    let second_token = "2".repeat(64);
    home.account(&first.origin, &first_token);
    home.account("https://other.example", &second_token);
    home.success(&["config", "set", "server", "https://other.example"]);
    home.remember_target(&first.origin, "alice");
    home.success(&["whoami", "--server", &first.origin, "--json"]);
    let request = first.request();
    assert!(request.starts_with("GET /api/client/account/alice "));
    assert!(!request.to_ascii_lowercase().contains("authorization:"));
    assert!(!request.contains(&first_token));
    assert!(!request.contains(&second_token));

    let redirect = Server::once("302 Found", json!({}));
    home.account(&redirect.origin, &first_token);
    home.remember_target(&redirect.origin, "alice");
    let output = home.command(&["whoami", "--server", &redirect.origin]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("redirected"));
    assert!(
        redirect
            .request()
            .starts_with("GET /api/client/account/alice ")
    );
}

#[test]
fn invalid_configuration_never_silently_selects_the_builtin_server() {
    let home = Home::new();
    fs::write(
        home.0.join("config.toml"),
        "[client]\nserver = 'http://remote.example'\n",
    )
    .unwrap();
    for args in [vec!["config", "get", "server"], vec!["whoami"]] {
        let result = home.command(&args);
        assert!(!result.status.success());
        assert!(String::from_utf8_lossy(&result.stderr).contains("HTTPS origin"));
    }
    for args in [vec![], vec!["--no-watchdog"]] {
        let result = home.command(&args);
        assert!(!result.status.success());
        assert!(result.stdout.is_empty());
        assert!(String::from_utf8_lossy(&result.stderr).contains("ssmcp username@server"));
    }
    fs::write(home.0.join("config.toml"), "[client]\nserver = 123\n").unwrap();
    let result = home.command(&["whoami"]);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("TOML"));
}

#[test]
fn missing_target_never_contacts_the_saved_relay() {
    let home = Home::new();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    home.account(&origin, &"a".repeat(64));
    home.remember_target(&origin, "alice");
    for args in [
        vec![],
        vec!["--no-watchdog"],
        vec!["--username", "alice"],
        vec!["--server", &origin],
    ] {
        let output = home.command(&args);
        assert!(!output.status.success(), "{args:?}");
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr).contains("ssmcp username@server"));
    }
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    assert!(!home.0.join("sessions").exists());
}
