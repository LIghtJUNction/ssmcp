//! Real first-connection flows in an isolated home, with a loopback portal and
//! the installed OpenSSH tools. These tests never install dependencies or open
//! a real browser, and never replace HOME or read the user's SSH configuration.
#![cfg(unix)]

use super::*;
use axum::{
    Json, Router,
    extract::State,
    http::{StatusCode, Uri},
    routing::post,
};
use std::{
    collections::VecDeque,
    fs,
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    sync::{Arc, Mutex},
};
use tokio::task::JoinHandle;

struct Sandbox {
    root: PathBuf,
    home: PathBuf,
    config: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let mut bytes = [0_u8; 16];
        getrandom::fill(&mut bytes).expect("temporary fixture entropy");
        let nonce: String = bytes.iter().map(|v| format!("{v:02x}")).collect();
        let root = std::env::temp_dir().join(format!(
            "ssmcp-onboarding-flow-{}-{nonce}",
            std::process::id()
        ));
        fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
        let home = root.join("isolated home");
        fs::DirBuilder::new().mode(0o700).create(&home).unwrap();
        Self {
            config: root
                .join("separate SSH configuration")
                .join("connections.conf"),
            root,
            home,
        }
    }

    fn options(&self) -> Options {
        Options::parse([
            OsString::from("--no-color"),
            OsString::from("-F"),
            self.config.as_os_str().to_owned(),
            OsString::from("-p"),
            OsString::from("2222"),
            OsString::from("setup.example.invalid"),
        ])
        .unwrap()
        .unwrap()
    }

    fn target(&self, portal: &Portal) -> Target {
        Target {
            host: "setup.example.invalid".into(),
            port: 2222,
            portal: portal.state.base.clone(),
            config: self.config.clone(),
        }
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[derive(Default)]
struct FakeConsole {
    answers: VecDeque<String>,
    prompts: Vec<String>,
    messages: Vec<String>,
    opened: Vec<String>,
}

impl FakeConsole {
    fn answering(answers: &[&str]) -> Self {
        Self {
            answers: answers.iter().map(|v| (*v).to_owned()).collect(),
            ..Self::default()
        }
    }
}

impl Console for FakeConsole {
    async fn line(&mut self, prompt: &str) -> Result<String, String> {
        // Even if a binary vanishes after the preflight, this fixture can never
        // give consent to an installer.
        assert!(
            !prompt.contains("安装") && !prompt.to_ascii_lowercase().contains("install"),
            "flow tests must never enter dependency installation"
        );
        self.prompts.push(prompt.to_owned());
        self.answers
            .pop_front()
            .ok_or_else(|| format!("unexpected console prompt: {prompt}"))
    }

    async fn open(&mut self, url: &str) -> Result<(), String> {
        self.opened.push(url.to_owned());
        Ok(())
    }

    fn say(&mut self, message: &str) {
        self.messages.push(message.to_owned());
    }
}

async fn openssh_output(
    program: &Path,
    arguments: &[OsString],
) -> Result<std::process::Output, String> {
    let mut command = Command::new(program);
    command
        .args(arguments)
        .stdin(Stdio::null())
        .kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(10), command.output())
        .await
        .map_err(|_| "fixture OpenSSH process timed out".to_owned())?
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(format!(
            "fixture OpenSSH failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(output)
}

async fn require_system_openssh() -> dependencies::SshPrograms {
    let programs = dependencies::discover()
        .expect("flow tests require installed ssh and ssh-keygen; they never install either");
    assert!(programs.ssh.is_file() && programs.keygen.is_file());
    let version = openssh_output(&programs.ssh, &[OsString::from("-V")])
        .await
        .expect("system ssh must execute before complete is called");
    assert!(
        String::from_utf8_lossy(&version.stderr).contains("OpenSSH")
            || String::from_utf8_lossy(&version.stdout).contains("OpenSSH"),
        "these tests require the real OpenSSH client"
    );
    programs
}

fn account_json() -> Value {
    json!({"github_login":"OctoCat","username":"gh-42","keys":[]})
}

fn account(value: Value) -> Account {
    serde_json::from_value(value).unwrap()
}

#[derive(Clone, Debug)]
struct Request {
    route: String,
    body: Value,
}

struct PortalState {
    base: Url,
    polls: Mutex<VecDeque<Value>>,
    requests: Mutex<Vec<Request>>,
    key_account: Value,
    keygen: PathBuf,
    uploaded_public: PathBuf,
}

struct Portal {
    state: Arc<PortalState>,
    task: JoinHandle<std::io::Result<()>>,
}

impl Portal {
    async fn new(
        sandbox: &Sandbox,
        programs: &dependencies::SshPrograms,
        polls: Vec<Value>,
        key_account: Value,
    ) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = portal_url(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let state = Arc::new(PortalState {
            base,
            polls: Mutex::new(polls.into()),
            requests: Mutex::new(Vec::new()),
            key_account,
            keygen: programs.keygen.clone(),
            uploaded_public: sandbox.root.join("portal-received-public.pub"),
        });
        let app = Router::new()
            .route("/api/setup/{step}", post(portal_request))
            .with_state(state.clone());
        let task = tokio::spawn(async move { axum::serve(listener, app).await });
        Self { state, task }
    }

    fn api(&self) -> Api {
        // Literal loopback remains local even when the developer has configured
        // HTTP_PROXY. The production post/poll/complete paths are unchanged.
        Api {
            base: self.state.base.clone(),
            client: Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(2))
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
        }
    }

    async fn start(&self, api: &Api) -> Start {
        serde_json::from_value(
            api.post("api/setup/start", json!({"client_name":"flow-fixture"}))
                .await
                .unwrap(),
        )
        .unwrap()
    }

    fn requests(&self) -> Vec<Request> {
        self.state.requests.lock().unwrap().clone()
    }
}

impl Drop for Portal {
    fn drop(&mut self) {
        self.task.abort();
    }
}

type HttpError = (StatusCode, Json<Value>);

fn http_error(message: impl Into<String>) -> HttpError {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({"error":"fixture_rejected","message":message.into()})),
    )
}

async fn fingerprint_with_openssh(keygen: &Path, public: &Path) -> Result<String, String> {
    let output = openssh_output(
        keygen,
        &[
            OsString::from("-l"),
            OsString::from("-f"),
            public.as_os_str().to_owned(),
            OsString::from("-E"),
            OsString::from("sha256"),
        ],
    )
    .await?;
    let text = String::from_utf8(output.stdout).map_err(|e| e.to_string())?;
    text.split_whitespace()
        .nth(1)
        .filter(|fingerprint| fingerprint.starts_with("SHA256:"))
        .map(str::to_owned)
        .ok_or_else(|| "OpenSSH did not return a SHA256 fingerprint".into())
}

async fn portal_request(
    State(state): State<Arc<PortalState>>,
    uri: Uri,
    Json(body): Json<Value>,
) -> Result<Json<Value>, HttpError> {
    state.requests.lock().unwrap().push(Request {
        route: uri.to_string(),
        body: body.clone(),
    });
    match uri.path() {
        "/api/setup/start" => Ok(Json(json!({
            "device_code":"s".repeat(43),
            "user_code":"ABCDE-FGHIJ",
            "verification_uri_complete":state.base.join("setup?code=ABCDE-FGHIJ").unwrap().to_string(),
            "expires_in":30,
            "interval":1
        }))),
        "/api/setup/poll" => state
            .polls
            .lock()
            .unwrap()
            .pop_front()
            .map(Json)
            .ok_or_else(|| http_error("unexpected extra poll")),
        "/api/setup/key" => {
            let public = body["public_key"]
                .as_str()
                .filter(|v| v.starts_with("ssh-ed25519 ") && !v.contains('\n'))
                .ok_or_else(|| http_error("expected a single public key"))?;
            fs::write(&state.uploaded_public, format!("{public}\n"))
                .map_err(|e| http_error(e.to_string()))?;
            // Validate the uploaded public material with OpenSSH, independently
            // of onboarding's parser and fingerprint implementation.
            let fingerprint = fingerprint_with_openssh(&state.keygen, &state.uploaded_public)
                .await
                .map_err(http_error)?;
            let mut saved = state.key_account.clone();
            saved["keys"] = json!([{"public_key":public,"fingerprint":fingerprint}]);
            Ok(Json(json!({"status":"saved","account":saved})))
        }
        _ => Err(http_error("unexpected route")),
    }
}

async fn assert_effective_ssh_config(
    programs: &dependencies::SshPrograms,
    options: &Options,
    target: &Target,
    identity: &Path,
) {
    let alias = options.destination.as_deref().unwrap();
    let output = openssh_output(
        &programs.ssh,
        &[
            "-G".into(),
            "-F".into(),
            target.config.as_os_str().to_owned(),
            "-o".into(),
            "CanonicalizeHostname=no".into(),
            "--".into(),
            alias.into(),
        ],
    )
    .await
    .unwrap();
    let text = String::from_utf8(output.stdout).unwrap();
    for expected in [
        format!("hostname {}", target.host),
        "user gh-42".into(),
        format!("port {}", target.port),
        "identitiesonly yes".into(),
        format!("identityfile {}", identity.display()),
    ] {
        assert!(
            text.lines().any(|line| line == expected),
            "{expected} absent from ssh -G"
        );
    }
    assert_eq!(options.config.as_deref(), Some(target.config.as_os_str()));
    assert_eq!(options.identity.as_deref(), Some(identity.as_os_str()));
    assert_eq!(options.allow, ["octocat@github"]);
    let args: Vec<_> = options
        .command()
        .as_std()
        .get_args()
        .map(OsString::from)
        .collect();
    assert!(
        args.windows(2)
            .any(|pair| pair[0] == "-F" && pair[1] == target.config.as_os_str()),
        "resumed connection must use the saved config"
    );
    assert!(args.iter().any(|arg| arg == alias));
}

#[tokio::test]
async fn pending_authorization_creates_local_key_uploads_only_public_and_accepts_default_name() {
    let programs = require_system_openssh().await;
    let sandbox = Sandbox::new();
    let initial = account_json();
    let mut same_account = initial.clone();
    // GitHub login casing may change without changing the authorized identity.
    same_account["github_login"] = json!("octocat");
    let portal = Portal::new(
        &sandbox,
        &programs,
        vec![
            json!({"status":"pending"}),
            json!({"status":"authorized","account":initial}),
        ],
        same_account,
    )
    .await;
    let api = portal.api();
    let start = portal.start(&api).await;
    let mut console = FakeConsole::answering(&["yes", ""]);
    let verification = verification_url(
        &api.base,
        &start.verification_uri_complete,
        &start.user_code,
    )
    .unwrap();
    console.open(verification.as_str()).await.unwrap();
    let authorized = wait_authorized(&api, &start, &mut console).await.unwrap();
    let mut options = sandbox.options();
    let target = sandbox.target(&portal);
    complete(
        &mut options,
        &sandbox.home,
        &target,
        &api,
        &start,
        authorized,
        &mut console,
    )
    .await
    .unwrap();

    assert!(console.answers.is_empty());
    let accepted_name = options.name.as_deref().unwrap();
    assert!(!accepted_name.is_empty());
    assert!(
        console
            .prompts
            .last()
            .unwrap()
            .contains(&format!("[{accepted_name}]")),
        "empty input must retain the default name displayed in the prompt"
    );
    assert_eq!(console.opened, [verification.as_str()]);
    let private = PathBuf::from(options.identity.as_ref().unwrap());
    assert!(private.starts_with(&sandbox.home));
    assert!(public_file(&private).is_file());
    assert_eq!(
        fs::metadata(&private).unwrap().permissions().mode() & 0o077,
        0
    );
    assert_eq!(
        fs::metadata(&target.config).unwrap().permissions().mode() & 0o077,
        0
    );
    assert!(
        !sandbox.home.join(".ssh/config").exists(),
        "custom config path must be honored"
    );

    let derived = openssh_output(
        &programs.keygen,
        &[
            "-y".into(),
            "-P".into(),
            "".into(),
            "-f".into(),
            private.as_os_str().to_owned(),
        ],
    )
    .await
    .unwrap();
    let derived = String::from_utf8(derived.stdout).unwrap();
    let canonical = derived
        .split_whitespace()
        .take(2)
        .collect::<Vec<_>>()
        .join(" ");
    let requests = portal.requests();
    assert_eq!(
        requests
            .iter()
            .map(|r| r.route.as_str())
            .collect::<Vec<_>>(),
        [
            "/api/setup/start",
            "/api/setup/poll",
            "/api/setup/poll",
            "/api/setup/key"
        ]
    );
    for request in &requests[1..] {
        assert_eq!(request.body["device_code"], start.device_code);
        assert!(!request.route.contains(&start.device_code));
    }
    let upload = &requests.last().unwrap().body;
    let mut fields: Vec<_> = upload
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    fields.sort_unstable();
    assert_eq!(fields, ["device_code", "label", "public_key"]);
    assert_eq!(upload["public_key"], canonical);
    let private_bytes = fs::read(&private).unwrap();
    assert!(private_bytes.starts_with(b"-----BEGIN OPENSSH PRIVATE KEY-----"));
    let wire = serde_json::to_string(upload).unwrap();
    assert!(!wire.contains("PRIVATE KEY"));
    assert!(!wire.contains(std::str::from_utf8(&private_bytes).unwrap()));
    assert_effective_ssh_config(&programs, &options, &target, &private).await;
}

#[tokio::test]
async fn declining_creation_does_not_generate_or_upload_a_key_or_write_config() {
    let programs = require_system_openssh().await;
    let sandbox = Sandbox::new();
    let portal = Portal::new(&sandbox, &programs, vec![], account_json()).await;
    let api = portal.api();
    let start = portal.start(&api).await;
    let mut options = sandbox.options();
    let target = sandbox.target(&portal);
    let mut console = FakeConsole::answering(&["no"]);
    let error = complete(
        &mut options,
        &sandbox.home,
        &target,
        &api,
        &start,
        account(account_json()),
        &mut console,
    )
    .await
    .unwrap_err();
    assert!(error.contains("declined"), "{error}");
    assert!(!managed_key(&sandbox.home, &target).exists());
    assert!(!public_file(&managed_key(&sandbox.home, &target)).exists());
    assert!(!target.config.exists());
    assert!(options.identity.is_none() && options.name.is_none() && options.allow.is_empty());
    assert_eq!(
        portal.requests().len(),
        1,
        "declining must not upload anything"
    );
    assert!(console.answers.is_empty());
}

#[tokio::test]
async fn browser_denial_stops_before_key_creation_or_config_changes() {
    let programs = require_system_openssh().await;
    let sandbox = Sandbox::new();
    let portal = Portal::new(
        &sandbox,
        &programs,
        vec![json!({"status":"denied"})],
        account_json(),
    )
    .await;
    let api = portal.api();
    let start = portal.start(&api).await;
    let mut console = FakeConsole::default();
    let error = wait_authorized(&api, &start, &mut console)
        .await
        .err()
        .unwrap();
    assert!(error.contains("declined"), "{error}");
    assert!(!sandbox.home.join(".ssh").exists());
    assert!(!sandbox.config.exists());
    assert!(console.prompts.is_empty());
    assert_eq!(
        portal
            .requests()
            .iter()
            .map(|r| r.route.as_str())
            .collect::<Vec<_>>(),
        ["/api/setup/start", "/api/setup/poll"]
    );
}

#[tokio::test]
async fn registered_local_key_is_reused_without_upload_and_repeated_config_is_identical() {
    let programs = require_system_openssh().await;
    let sandbox = Sandbox::new();
    let identity = sandbox.home.join("existing identity");
    openssh_output(
        &programs.keygen,
        &[
            "-q".into(),
            "-t".into(),
            "ed25519".into(),
            "-N".into(),
            "".into(),
            "-f".into(),
            identity.as_os_str().to_owned(),
        ],
    )
    .await
    .unwrap();
    let before_private = fs::read(&identity).unwrap();
    let raw_public = fs::read_to_string(public_file(&identity)).unwrap();
    let public = raw_public
        .split_whitespace()
        .take(2)
        .collect::<Vec<_>>()
        .join(" ");
    let fingerprint = fingerprint_with_openssh(&programs.keygen, &public_file(&identity))
        .await
        .unwrap();
    let mut registered = account_json();
    registered["keys"] = json!([{"public_key":public,"fingerprint":fingerprint}]);
    let portal = Portal::new(&sandbox, &programs, vec![], registered.clone()).await;
    let api = portal.api();
    let start = portal.start(&api).await;
    let target = sandbox.target(&portal);
    let mut options = sandbox.options();
    options.identity = Some(identity.as_os_str().to_owned());
    options.name = Some("existing-workstation".into());
    fs::DirBuilder::new()
        .mode(0o700)
        .create(target.config.parent().unwrap())
        .unwrap();
    let original = "Host old-project\n    HostName retained.example.invalid\n    User original\nHost *\n    ServerAliveInterval 23\n";
    fs::write(&target.config, original).unwrap();
    fs::set_permissions(&target.config, fs::Permissions::from_mode(0o600)).unwrap();

    let mut console = FakeConsole::answering(&["", ""]);
    complete(
        &mut options,
        &sandbox.home,
        &target,
        &api,
        &start,
        account(registered.clone()),
        &mut console,
    )
    .await
    .unwrap();
    let first_config = fs::read(&target.config).unwrap();
    assert!(String::from_utf8_lossy(&first_config).ends_with(original));
    complete(
        &mut options,
        &sandbox.home,
        &target,
        &api,
        &start,
        account(registered),
        &mut console,
    )
    .await
    .unwrap();

    assert_eq!(fs::read(&target.config).unwrap(), first_config);
    assert!(
        fs::read(&identity).unwrap() == before_private,
        "existing private key must not change"
    );
    assert!(!managed_key(&sandbox.home, &target).exists());
    assert_eq!(options.name.as_deref(), Some("existing-workstation"));
    assert!(console.answers.is_empty());
    assert_eq!(
        console.prompts.len(),
        2,
        "registered identity only needs a computer-name prompt"
    );
    assert_eq!(
        portal.requests().len(),
        1,
        "registered key must never be uploaded again"
    );
    let backups: Vec<_> = fs::read_dir(target.config.parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("config.ssmcp-backup-")
        })
        .collect();
    assert_eq!(
        backups.len(),
        1,
        "idempotent setup must not create another backup"
    );
    assert_eq!(fs::read_to_string(&backups[0]).unwrap(), original);
    assert_effective_ssh_config(&programs, &options, &target, &identity).await;
}

#[tokio::test]
async fn invalid_explicit_identity_never_falls_back_to_a_registered_default_key() {
    let programs = require_system_openssh().await;
    for invalid_public in [None, Some("not an Ed25519 public key\n")] {
        let sandbox = Sandbox::new();
        let ssh_directory = sandbox.home.join(".ssh");
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&ssh_directory)
            .unwrap();
        let default_identity = ssh_directory.join("id_ed25519");
        openssh_output(
            &programs.keygen,
            &[
                "-q".into(),
                "-t".into(),
                "ed25519".into(),
                "-N".into(),
                "".into(),
                "-f".into(),
                default_identity.as_os_str().to_owned(),
            ],
        )
        .await
        .unwrap();
        let original_private = fs::read(&default_identity).unwrap();
        let raw_public = fs::read_to_string(public_file(&default_identity)).unwrap();
        let public = raw_public
            .split_whitespace()
            .take(2)
            .collect::<Vec<_>>()
            .join(" ");
        let fingerprint =
            fingerprint_with_openssh(&programs.keygen, &public_file(&default_identity))
                .await
                .unwrap();
        let mut registered = account_json();
        registered["keys"] = json!([{"public_key":public,"fingerprint":fingerprint}]);

        // The explicit private file is real, but its corresponding public file
        // is missing or invalid. A separate default key is fully usable and
        // already registered, so the old fallback path would continue to naming.
        let explicit_identity = sandbox.home.join("explicit identity");
        fs::copy(&default_identity, &explicit_identity).unwrap();
        if let Some(contents) = invalid_public {
            fs::write(public_file(&explicit_identity), contents).unwrap();
        }
        let portal = Portal::new(&sandbox, &programs, vec![], registered.clone()).await;
        let api = portal.api();
        let start = portal.start(&api).await;
        let target = sandbox.target(&portal);
        let mut options = sandbox.options();
        options.identity = Some(explicit_identity.as_os_str().to_owned());
        // Answers would allow a mistakenly selected registered default key to
        // finish. Correct rejection must not ask for any input at all.
        let mut console = FakeConsole::answering(&["", "yes"]);
        let error = complete(
            &mut options,
            &sandbox.home,
            &target,
            &api,
            &start,
            account(registered),
            &mut console,
        )
        .await
        .unwrap_err();

        assert!(
            error.contains(explicit_identity.to_str().unwrap()),
            "explicit identity diagnostic must name the requested path: {error}"
        );
        assert!(
            console.prompts.is_empty(),
            "an invalid explicit key must stop before consent or naming"
        );
        assert_eq!(console.answers.len(), 2);
        assert_eq!(
            portal
                .requests()
                .iter()
                .map(|request| request.route.as_str())
                .collect::<Vec<_>>(),
            ["/api/setup/start"],
            "an invalid explicit key must never be uploaded or replaced by a default key"
        );
        assert!(!portal.state.uploaded_public.exists());
        assert!(!target.config.exists());
        assert!(!managed_key(&sandbox.home, &target).exists());
        assert!(!public_file(&managed_key(&sandbox.home, &target)).exists());
        assert_eq!(
            options.identity.as_deref(),
            Some(explicit_identity.as_os_str())
        );
        assert!(options.name.is_none() && options.allow.is_empty());
        assert_eq!(
            options.destination.as_deref(),
            Some("setup.example.invalid")
        );
        assert_eq!(fs::read(&default_identity).unwrap(), original_private);
        assert_eq!(fs::read(&explicit_identity).unwrap(), original_private);
        assert_eq!(
            fs::read(public_file(&explicit_identity)).ok(),
            invalid_public.map(|contents| contents.as_bytes().to_vec())
        );
    }
}

#[tokio::test]
async fn key_response_cannot_swap_authorized_github_login_or_ssh_username() {
    let programs = require_system_openssh().await;
    for (field, replacement) in [("github_login", "another-user"), ("username", "gh-99")] {
        let sandbox = Sandbox::new();
        let mut swapped = account_json();
        swapped[field] = json!(replacement);
        let portal = Portal::new(&sandbox, &programs, vec![], swapped).await;
        let api = portal.api();
        let start = portal.start(&api).await;
        let target = sandbox.target(&portal);
        let mut options = sandbox.options();
        let mut console = FakeConsole::answering(&["yes"]);
        let error = complete(
            &mut options,
            &sandbox.home,
            &target,
            &api,
            &start,
            account(account_json()),
            &mut console,
        )
        .await
        .unwrap_err();
        assert!(
            error.contains("changed the authorized account"),
            "{field}: {error}"
        );
        assert!(
            !target.config.exists(),
            "{field} swap must be rejected before config is saved"
        );
        assert!(
            managed_key(&sandbox.home, &target).is_file(),
            "generated private key stays local for recovery"
        );
        assert!(options.identity.is_none() && options.name.is_none() && options.allow.is_empty());
        assert_eq!(
            options.destination.as_deref(),
            Some("setup.example.invalid")
        );
        assert_eq!(
            portal
                .requests()
                .iter()
                .map(|r| r.route.as_str())
                .collect::<Vec<_>>(),
            ["/api/setup/start", "/api/setup/key"]
        );
        assert_eq!(
            console.prompts.len(),
            1,
            "account mismatch must stop before naming/config"
        );
    }
}
