//! Isolated public CLI checks. No real account, server or session is contacted.
#![cfg(unix)]
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    ffi::OsStr,
    fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::{Command, Output},
    time::{SystemTime, UNIX_EPOCH},
};

struct Home(PathBuf);
impl Home {
    fn new() -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "ssmcp-connection-context-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        fs::create_dir(path.join("auth")).unwrap();
        fs::set_permissions(path.join("auth"), fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(path.join("config.toml"), "# keep preferences\n[ui]\ntheme = 'nord' # user theme\n[browser]\nbackground = false\n[git]\nfetch_interval_seconds = 600\n").unwrap();
        Self(path)
    }

    fn command(&self, args: &[&str], selected: Option<&OsStr>) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ssmcp"));
        command
            .args(args)
            .env("SSMCP_CLIENT_CONFIG", self.0.join("config.toml"))
            .env("SSMCP_AUTH_HOME", self.0.join("auth"))
            .env("SSMCP_SESSION_HOME", self.0.join("sessions"))
            .env_remove("SSMCP_CONTEXT")
            .env_remove("SSMCP_UI_CONFIG")
            .env_remove("SSMCP_RELAY_TOKEN")
            .env("NO_COLOR", "1");
        if let Some(selected) = selected {
            command.env("SSMCP_CONTEXT", selected);
        }
        command.output().unwrap()
    }

    fn success(&self, args: &[&str], selected: Option<&str>) -> String {
        let output = self.command(args, selected.map(OsStr::new));
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    fn json(&self, args: &[&str], selected: Option<&str>) -> Value {
        serde_json::from_str(&self.success(args, selected)).unwrap()
    }

    fn failure(&self, args: &[&str], selected: Option<&str>, message: &str) {
        let output = self.command(args, selected.map(OsStr::new));
        assert!(!output.status.success(), "unexpected success: {args:?}");
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.contains(message), "{args:?}: {stderr}");
    }

    fn contexts(&self) {
        self.success(
            &[
                "context",
                "create",
                "work",
                "--server",
                "https://WORK.example:443/",
            ],
            None,
        );
        self.success(
            &[
                "context",
                "create",
                "other",
                "--server",
                "https://other.example:8443",
            ],
            None,
        );
        self.success(&["context", "create", "here", "--local"], None);
        self.success(&["context", "use", "work"], None);
    }
}
impl Drop for Home {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn context_cli_precedence_reports_selection_without_persisting_overrides() {
    let home = Home::new();
    home.contexts();
    let before = fs::read(home.0.join("config.toml")).unwrap();
    let saved = home.json(&["context", "show", "--json"], None);
    assert_eq!(
        saved,
        json!({"name":"work", "endpoint":"https://work.example", "selection_source":"current"})
    );
    let environment = home.json(&["context", "inspect", "--json"], Some("other"));
    assert_eq!(environment["name"], "other");
    assert_eq!(environment["selection_source"], "environment");
    let explicit = home.json(
        &["--context=here", "context", "show", "--json"],
        Some("missing"),
    );
    assert_eq!(explicit["endpoint"], "local");
    assert_eq!(explicit["selection_source"], "command_line");
    let named = home.json(&["context", "inspect", "other"], Some("missing"));
    assert_eq!(named["selection_source"], "argument");
    let list = home.json(&["context", "list", "--json"], Some("other"));
    let work = list
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["name"] == "work")
        .unwrap();
    assert_eq!(work["current"], true);
    assert_eq!(work["selected"], false);
    assert_eq!(fs::read(home.0.join("config.toml")).unwrap(), before);
    let text = String::from_utf8(before).unwrap();
    assert!(text.contains("# keep preferences"));
    assert!(text.contains("theme = 'nord' # user theme"));
    assert!(text.contains("background = false"));
    assert!(text.contains("fetch_interval_seconds = 600"));
    assert_eq!(
        fs::metadata(home.0.join("config.toml"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    home.success(&["context", "use", "--default"], None);
    home.success(
        &["config", "set", "server", "https://fallback.example"],
        None,
    );
    assert_eq!(
        home.json(&["context", "show", "--json"], None),
        json!({"name":null, "endpoint":"https://fallback.example", "selection_source":"legacy"})
    );
}

#[test]
fn context_cli_rejects_invalid_endpoints_unknown_names_and_destructive_ambiguity() {
    let home = Home::new();
    home.contexts();
    let before = fs::read(home.0.join("config.toml")).unwrap();
    for endpoint in [
        "local",
        "ssh://host",
        "http://127.0.0.1:9000",
        "https://user:secret@host",
        "https://host/path",
        "https://host?token=secret",
    ] {
        let output = home.command(&["context", "create", "bad", "--server", endpoint], None);
        assert!(!output.status.success(), "{endpoint}");
        assert!(!String::from_utf8_lossy(&output.stderr).contains("secret"));
    }
    home.failure(
        &["context", "use", "missing"],
        None,
        "unknown connection context",
    );
    home.failure(
        &["context", "create", "work", "--local"],
        None,
        "already exists",
    );
    home.failure(
        &["context", "rm", "work"],
        None,
        "cannot remove the current context",
    );
    home.failure(
        &["context", "show", "--json"],
        Some("missing"),
        "unknown connection context",
    );
    assert_eq!(fs::read(home.0.join("config.toml")).unwrap(), before);
    home.success(&["context", "use", "here"], None);
    home.success(&["context", "rm", "work"], None);
    home.failure(
        &["context", "inspect", "work"],
        None,
        "unknown connection context",
    );
}

#[test]
fn context_cli_unknown_and_conflicting_selectors_fail_before_account_dispatch() {
    let home = Home::new();
    home.contexts();
    for args in [
        vec!["--context", "missing"],
        vec!["--context=missing", "whoami", "--json"],
        vec!["list", "--context", "missing", "--json"],
        vec!["--context", "missing", "-i"],
        vec!["trust", "remove", "--context", "missing"],
        vec!["sessions", "create", "saved", "--context", "missing"],
    ] {
        home.failure(&args, None, "unknown connection context");
    }
    for args in [
        vec!["--context", "work", "--server", "https://other.example"],
        vec!["--context", "work", "alice@work.example"],
        vec!["local", "--context", "here"],
        vec!["--context", "here", "local"],
        vec![
            "logout",
            "--context",
            "work",
            "--server",
            "https://work.example",
        ],
        vec![
            "--context",
            "work",
            "trust",
            "remove",
            "--server",
            "https://work.example",
        ],
    ] {
        home.failure(&args, None, "cannot be combined");
    }
    home.failure(&["--context", "work", "--context=work"], None, "only once");
    home.failure(
        &["logout", "--context", "work", "--context=work"],
        None,
        "only once",
    );
    home.failure(
        &["--context", "work", "sessions", "restart", "saved"],
        None,
        "recorded target",
    );
    home.failure(
        &["--context", "work", "config", "get", "server"],
        None,
        "local management command",
    );
}

#[test]
fn context_local_account_commands_never_fall_back_to_remote() {
    let home = Home::new();
    home.contexts();
    for args in [
        vec!["login"],
        vec!["whoami", "--json"],
        vec!["logout", "--json"],
        vec!["list", "--json"],
        vec!["-i"],
        vec!["sessions", "list", "--remote"],
        vec!["trust", "remove"],
    ] {
        home.failure(&args, Some("here"), "selected context is local");
    }
    let explicit = home.json(
        &["logout", "--server", "https://explicit.example", "--json"],
        Some("missing"),
    );
    assert_eq!(explicit["server"], "https://explicit.example");
    let selected = home.json(&["--context", "work", "logout", "--json"], Some("here"));
    assert_eq!(selected["server"], "https://work.example");
}

#[cfg(target_os = "linux")]
#[test]
fn context_local_options_keep_loopback_foreground_and_context_root_rules() {
    let home = Home::new();
    home.contexts();
    home.failure(
        &["--context", "here", "--listen", "0.0.0.0:0"],
        None,
        "only binds literal loopback",
    );
    home.failure(
        &["--context", "here", "--public-url", "https://sample.local"],
        None,
        "fixed nonzero",
    );
    home.failure(
        &["--context", "here", "--daemon"],
        None,
        "foreground OS-user",
    );
    home.failure(
        &["--context", "here", "--username", "alice"],
        None,
        "foreground OS-user",
    );
    home.failure(
        &["--context", "here", "--allow", "alice@work.example"],
        None,
        "foreground OS-user",
    );
    home.failure(
        &[
            "--context",
            "work",
            "--context-root",
            "/path-that-does-not-exist-ssmcp-context",
        ],
        None,
        "--context-root",
    );
    home.failure(
        &["local", "--daemon"],
        Some("missing"),
        "foreground OS-user",
    );
    home.failure(
        &["--ssh", "legacy-host"],
        Some("here"),
        "SSH device connections have been removed",
    );
    home.failure(
        &["--context", "work", "--title", "--context"],
        None,
        "choose a username to connect",
    );
}

#[test]
fn context_account_credentials_stay_scoped_to_the_concrete_origin() {
    let home = Home::new();
    home.contexts();
    let foreign = "https://other.example:8443";
    let file = home
        .0
        .join("auth")
        .join(format!("{:x}.json", Sha256::digest(foreign.as_bytes())));
    let credential = json!({"server":foreign,"token":"f".repeat(64),"principal":"alice@github","subject":"123","expires_at":1});
    fs::write(&file, serde_json::to_vec(&credential).unwrap()).unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
    let default = home.0.join("auth/default");
    fs::write(&default, foreign).unwrap();
    fs::set_permissions(&default, fs::Permissions::from_mode(0o600)).unwrap();
    let before = fs::read(&file).unwrap();
    let output = home.success(&["--context", "work", "logout", "--json"], None);
    assert_eq!(
        serde_json::from_str::<Value>(&output).unwrap()["server"],
        "https://work.example"
    );
    assert!(!output.contains(&"f".repeat(64)));
    assert_eq!(fs::read(&file).unwrap(), before);
    assert_eq!(fs::read_to_string(&default).unwrap(), foreign);
    let config = fs::read_to_string(home.0.join("config.toml")).unwrap();
    assert!(!config.contains("token"));
    assert!(!config.contains("principal"));
}

#[test]
fn context_cli_non_unicode_environment_cannot_choose_a_fallback() {
    use std::os::unix::ffi::OsStrExt;
    let home = Home::new();
    home.contexts();
    let invalid = OsStr::from_bytes(&[0xff]);
    let output = home.command(&["context", "show", "--json"], Some(invalid));
    assert!(!output.status.success());
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("SSMCP_CONTEXT must be UTF-8")
    );
    let output = home.command(
        &["--context", "work", "context", "show", "--json"],
        Some(invalid),
    );
    assert!(output.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap()["name"],
        "work"
    );
}
