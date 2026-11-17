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
            .env_remove("SSMCP_UI_CONFIG")
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
    fn account(&self, server: &str, token: &str) {
        let expires = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 3600;
        let path = self
            .0
            .join("auth")
            .join(format!("{:x}.json", Sha256::digest(server.as_bytes())));
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
    assert!(String::from_utf8_lossy(&denied.stderr).contains("not signed in"));
    let connection = home.command(&["--no-watchdog", "--name", "config-check"]);
    assert!(!connection.status.success());
    assert!(String::from_utf8_lossy(&connection.stderr).contains("not logged in"));
    home.success(&["config", "unset", "server"]);
    assert_eq!(
        home.success(&["config", "get", "server"]).trim(),
        "https://previous.example"
    );
}

#[test]
fn invite_override_uses_selected_origin_for_api_script_and_join() {
    let home = Home::new();
    home.success(&["config", "set", "server", "https://configured.example"]);
    let server = Server::once(
        "200 OK",
        json!({
            "token": format!("ci_{}", "b".repeat(64)), "expires_in": 600, "session_expires_in": 3600
        }),
    );
    let token = "c".repeat(64);
    home.account(&server.origin, &token);
    let output = home.success(&["--server", &server.origin, "invite", "--json"]);
    let invitation: serde_json::Value = serde_json::from_str(&output).unwrap();
    assert_eq!(invitation["server"], server.origin);
    assert_eq!(invitation["bootstrap"], true);
    let command = invitation["command_sh"].as_str().unwrap();
    assert!(command.contains(&format!("{}/client/install.sh", server.origin)));
    assert!(command.contains(&format!("--server '{}'", server.origin)));
    assert!(!command.contains("ssmcp.lmm.best"));
    assert!(!command.contains("configured.example"));
    assert!(!command.contains(&token));
    let request = server.request();
    assert!(request.starts_with("POST /api/client/invites "));
    assert!(
        request
            .to_ascii_lowercase()
            .contains(&format!("authorization: bearer {token}"))
    );
    // Per-command selection leaves the saved default untouched.
    assert_eq!(
        home.success(&["config", "get", "server"]).trim(),
        "https://configured.example"
    );
}

#[test]
fn configured_default_is_used_by_invite_login_and_join() {
    let home = Home::new();
    let server = Server::once(
        "200 OK",
        json!({
            "token": format!("ci_{}", "b".repeat(64)), "expires_in": 600, "session_expires_in": 3600
        }),
    );
    home.account(&server.origin, &"d".repeat(64));
    home.account("https://last-login.example", &"e".repeat(64));
    home.success(&["config", "set", "server", &server.origin]);
    let invitation: serde_json::Value =
        serde_json::from_str(&home.success(&["invite", "--json", "--no-bootstrap"])).unwrap();
    assert_eq!(invitation["server"], server.origin);
    assert_eq!(invitation["bootstrap"], false);
    assert!(server.request().starts_with("POST /api/client/invites "));

    let login = Server::once("400 Bad Request", json!({"error": "stop_test"}));
    home.success(&["config", "set", "server", &login.origin]);
    let output = home.command(&["login", "--no-browser"]);
    assert!(!output.status.success());
    assert!(login.request().starts_with("POST /api/client/login/start "));

    let join = Server::once("410 Gone", json!({"error": "expired"}));
    home.success(&["config", "set", "server", &join.origin]);
    let token = format!("ci_{}", "f".repeat(64));
    let output = home.command(&["join", "--token", &token, "--no-watchdog"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("expired"));
    let request = join.request();
    assert!(request.starts_with("POST /api/client/enroll "));
    assert!(!request.to_ascii_lowercase().contains("authorization:"));
    assert!(request.contains(&token));
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
    home.success(&["whoami", "--server", &first.origin, "--json"]);
    let request = first.request();
    assert!(request.contains(&first_token));
    assert!(!request.contains(&second_token));

    let redirect = Server::once("302 Found", json!({}));
    home.account(&redirect.origin, &first_token);
    let output = home.command(&["whoami", "--server", &redirect.origin]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("redirected"));
    assert!(redirect.request().starts_with("GET /api/client/whoami "));
}

#[test]
fn invalid_configuration_never_silently_selects_the_builtin_server() {
    let home = Home::new();
    fs::write(
        home.0.join("config.toml"),
        "[client]\nserver = 'http://remote.example'\n",
    )
    .unwrap();
    for args in [
        vec!["config", "get", "server"],
        vec!["login", "--no-browser"],
        vec!["invite"],
    ] {
        let result = home.command(&args);
        assert!(!result.status.success());
        assert!(String::from_utf8_lossy(&result.stderr).contains("HTTPS origin"));
    }
    fs::write(home.0.join("config.toml"), "[client]\nserver = 123\n").unwrap();
    let result = home.command(&["whoami"]);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("TOML"));
}
