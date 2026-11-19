use super::*;
use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt, symlink},
    path::PathBuf,
    process::{Child, Command, Stdio},
};

const ISSUER: &str = "https://ssmcp.example";
const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let path = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(random("ssmcp-grants-").ok().unwrap());
        fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        Self(path)
    }
    fn registry(&self) -> PathBuf {
        self.0.join("clients.toml")
    }
    fn grants(&self) -> PathBuf {
        self.0.join("clients.toml.grants.json")
    }
    fn pending(&self) -> PathBuf {
        self.0.join("clients.toml.grants.json.pending")
    }
    fn open(&self) -> crate::Result<OAuth> {
        let mut oauth = OAuth::new(ISSUER.into());
        oauth.enable_client_registry(&self.registry())?;
        Ok(oauth)
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn checked<T>(result: OResult<T>) -> T {
    result.unwrap_or_else(|e| panic!("{}: {}", e.code, e.description))
}
fn parameters(values: &[(&str, &str)]) -> Parameters {
    Parameters(
        values
            .iter()
            .map(|(k, v)| ((*k).into(), (*v).into()))
            .collect(),
    )
}
fn refresh(oauth: &OAuth, tokens: &Value) -> OResult<Value> {
    oauth.token(
        &parameters(&[
            ("grant_type", "refresh_token"),
            ("client_id", tokens["client_id"].as_str().unwrap()),
            ("refresh_token", tokens["refresh_token"].as_str().unwrap()),
            ("resource", "https://ssmcp.example/mcp"),
        ]),
        &HeaderMap::new(),
    )
}
fn account(oauth: &OAuth, tokens: &Value) -> Option<String> {
    match oauth.authenticate("mcp", tokens["access_token"].as_str().unwrap())? {
        Authorization::Identity(identity) => Some(identity.subject),
        Authorization::Legacy { .. } => None,
    }
}
fn original_deadline(oauth: &OAuth) -> u64 {
    oauth
        .state
        .lock()
        .unwrap()
        .families
        .values()
        .next()
        .unwrap()
        .absolute_expires_at_ms
}

#[test]
fn restart_restores_account_access_refresh_and_original_absolute_deadline() {
    let directory = Directory::new();
    let oauth = directory.open().unwrap();
    let (tokens, code) = oauth.test_identity_grant("mcp", "123");
    let absolute = original_deadline(&oauth);
    assert_eq!(account(&oauth, &tokens).as_deref(), Some("123"));
    let text = fs::read_to_string(directory.grants()).unwrap();
    for raw in [
        tokens["access_token"].as_str().unwrap(),
        tokens["refresh_token"].as_str().unwrap(),
        &code,
    ] {
        assert!(!text.contains(raw), "only hashes may be persisted");
    }
    assert_eq!(
        fs::metadata(directory.grants())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert!(!directory.pending().exists());
    drop(oauth);
    let oauth = directory.open().unwrap();
    assert_eq!(account(&oauth, &tokens).as_deref(), Some("123"));
    assert!(
        oauth
            .authenticate("other", tokens["access_token"].as_str().unwrap())
            .is_none()
    );
    assert_eq!(original_deadline(&oauth), absolute);
    let rotated = checked(refresh(&oauth, &tokens));
    assert_eq!(account(&oauth, &rotated).as_deref(), Some("123"));
    assert_eq!(original_deadline(&oauth), absolute);
    drop(oauth);
    let oauth = directory.open().unwrap();
    assert_eq!(account(&oauth, &rotated).as_deref(), Some("123"));
    assert_eq!(original_deadline(&oauth), absolute);
}

#[test]
fn refresh_replay_revocation_survives_another_restart_and_is_account_scoped() {
    let directory = Directory::new();
    let oauth = directory.open().unwrap();
    let alice = oauth.test_identity_tokens("mcp", "123");
    let bob = oauth.test_identity_tokens("mcp", "456");
    let mut rotated = checked(refresh(&oauth, &alice));
    rotated["client_id"] = alice["client_id"].clone();
    drop(oauth);
    let oauth = directory.open().unwrap();
    assert_eq!(refresh(&oauth, &alice).err().unwrap().code, "invalid_grant");
    assert!(account(&oauth, &rotated).is_none());
    assert_eq!(account(&oauth, &bob).as_deref(), Some("456"));
    drop(oauth);
    let oauth = directory.open().unwrap();
    assert!(account(&oauth, &alice).is_none());
    assert!(account(&oauth, &rotated).is_none());
    assert!(refresh(&oauth, &rotated).is_err());
    assert_eq!(account(&oauth, &bob).as_deref(), Some("456"));
}

#[test]
fn authorization_code_replay_keeps_its_pkce_and_resource_binding_after_restart() {
    let directory = Directory::new();
    let oauth = directory.open().unwrap();
    let (tokens, code) = oauth.test_identity_grant("mcp", "123");
    drop(oauth);
    let oauth = directory.open().unwrap();
    let mut args = parameters(&[
        ("grant_type", "authorization_code"),
        ("client_id", tokens["client_id"].as_str().unwrap()),
        ("code", &code),
        ("code_verifier", "wrong"),
        ("resource", "https://ssmcp.example/mcp"),
    ]);
    assert!(oauth.token(&args, &HeaderMap::new()).is_err());
    assert!(account(&oauth, &tokens).is_some());
    args.0.insert("code_verifier".into(), VERIFIER.into());
    args.0
        .insert("resource".into(), "https://ssmcp.example/other".into());
    assert!(oauth.token(&args, &HeaderMap::new()).is_err());
    assert!(account(&oauth, &tokens).is_some());
    args.0
        .insert("resource".into(), "https://ssmcp.example/mcp".into());
    assert_eq!(
        oauth.token(&args, &HeaderMap::new()).err().unwrap().code,
        "invalid_grant"
    );
    drop(oauth);
    assert!(account(&directory.open().unwrap(), &tokens).is_none());
}

#[test]
fn explicit_subject_revocation_remains_effective_after_restart() {
    let directory = Directory::new();
    let oauth = directory.open().unwrap();
    let alice = oauth.test_identity_tokens("mcp", "123");
    let bob = oauth.test_identity_tokens("mcp", "456");
    oauth.revoke_subject("123").unwrap();
    drop(oauth);
    let oauth = directory.open().unwrap();
    assert!(account(&oauth, &alice).is_none());
    assert!(refresh(&oauth, &alice).is_err());
    assert_eq!(account(&oauth, &bob).as_deref(), Some("456"));
}

#[test]
fn expired_persisted_grants_are_pruned_without_extending_their_lifetimes() {
    let directory = Directory::new();
    let oauth = directory.open().unwrap();
    let tokens = oauth.test_identity_tokens("mcp", "123");
    drop(oauth);
    let mut doc: Value = serde_json::from_slice(&fs::read(directory.grants()).unwrap()).unwrap();
    let expired = grants::wall_now_ms().unwrap() - 1000;
    for family in doc["families"].as_array_mut().unwrap() {
        family["expires_at_ms"] = json!(expired);
        family["absolute_expires_at_ms"] = json!(expired);
    }
    for kind in ["access", "refresh"] {
        for token in doc[kind].as_array_mut().unwrap() {
            token["expires_at_ms"] = json!(expired);
        }
    }
    fs::write(directory.grants(), serde_json::to_vec(&doc).unwrap()).unwrap();
    let oauth = directory.open().unwrap();
    assert!(account(&oauth, &tokens).is_none());
    assert!(refresh(&oauth, &tokens).is_err());
    assert!(oauth.state.lock().unwrap().families.is_empty());
    assert!(!oauth.state.lock().unwrap().clients.is_empty());
}

#[test]
fn grant_corruption_issuer_mismatch_and_incomplete_commit_fail_closed() {
    for variant in [
        "issuer",
        "future-clock",
        "duplicate",
        "orphan",
        "malformed",
        "pending",
    ] {
        let directory = Directory::new();
        let oauth = directory.open().unwrap();
        oauth.test_identity_tokens("mcp", "123");
        drop(oauth);
        let mut doc: Value =
            serde_json::from_slice(&fs::read(directory.grants()).unwrap()).unwrap();
        match variant {
            "issuer" => doc["issuer"] = json!("https://other.example"),
            "future-clock" => doc["written_at_ms"] = json!(grants::wall_now_ms().unwrap() + 60_000),
            "duplicate" => {
                let duplicate = doc["families"][0].clone();
                doc["families"].as_array_mut().unwrap().push(duplicate);
            }
            "orphan" => doc["access"][0]["family"] = json!(vec![0u8; 32]),
            "malformed" => {}
            "pending" => {
                let mut file = OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .mode(0o600)
                    .open(directory.pending())
                    .unwrap();
                file.write_all(b"incomplete test commit").unwrap();
            }
            _ => unreachable!(),
        }
        fs::write(
            directory.grants(),
            if variant == "malformed" {
                b"{invalid".to_vec()
            } else {
                serde_json::to_vec(&doc).unwrap()
            },
        )
        .unwrap();
        assert!(directory.open().is_err(), "{variant}");
    }
}

#[test]
fn grant_file_permissions_and_symlinks_are_checked_like_the_registry() {
    let directory = Directory::new();
    let oauth = directory.open().unwrap();
    oauth.test_identity_tokens("mcp", "123");
    drop(oauth);
    fs::set_permissions(directory.grants(), fs::Permissions::from_mode(0o644)).unwrap();
    assert!(directory.open().is_err());
    fs::set_permissions(directory.grants(), fs::Permissions::from_mode(0o600)).unwrap();
    let moved = directory.0.join("saved.json");
    fs::rename(directory.grants(), &moved).unwrap();
    symlink(&moved, directory.grants()).unwrap();
    assert!(directory.open().is_err());
}

#[test]
fn failed_snapshot_replacement_retains_intent_and_refuses_older_snapshot() {
    let directory = Directory::new();
    let oauth = directory.open().unwrap();
    let tokens = oauth.test_identity_tokens("mcp", "123");
    fs::set_permissions(directory.grants(), fs::Permissions::from_mode(0o644)).unwrap();
    let error = refresh(&oauth, &tokens).expect_err("write must fail");
    assert_eq!(error.status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        account(&oauth, &tokens).is_none(),
        "uncertain memory state is not accepted"
    );
    assert!(
        directory.pending().exists(),
        "durable intent survives the failed replacement"
    );
    drop(oauth);
    fs::set_permissions(directory.grants(), fs::Permissions::from_mode(0o600)).unwrap();
    assert!(
        directory.open().is_err(),
        "do not auto-accept the pre-rotation snapshot"
    );
}

struct Process(Child);
impl Process {
    fn stop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        self.stop();
    }
}

async fn start_process(directory: &Directory, generation: usize) -> (Process, String) {
    let address = directory.0.join(format!("address-{generation}.json"));
    let log = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(directory.0.join(format!("process-{generation}.log")))
        .unwrap();
    let mut process = Process(
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "oauth::grant_tests::grant_process_fixture",
                "--ignored",
                "--nocapture",
            ])
            .env("SSMCP_GRANT_PROCESS_DIRECTORY", &directory.0)
            .env("SSMCP_GRANT_PROCESS_ADDRESS", &address)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(log))
            .spawn()
            .unwrap(),
    );
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(bytes) = fs::read(&address) {
            let value: Value = serde_json::from_slice(&bytes).unwrap();
            return (process, value["base"].as_str().unwrap().into());
        }
        assert!(
            process.0.try_wait().unwrap().is_none(),
            "private core process exited"
        );
        assert!(
            tokio::time::Instant::now() < deadline,
            "private core process did not listen"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
async fn initialize(client: &reqwest::Client, base: &str, token: &str) -> StatusCode {
    client.post(format!("{base}/mcp")).bearer_auth(token)
        .header(header::ACCEPT,"application/json")
        .json(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
            "protocolVersion":"2026-07-28","capabilities":{},"clientInfo":{"name":"restart-fixture","version":"1"}
        }})).send().await.unwrap().status()
}
async fn http_refresh(client: &reqwest::Client, base: &str, tokens: &Value) -> reqwest::Response {
    client
        .post(format!("{base}/oauth/token"))
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", tokens["client_id"].as_str().unwrap()),
            ("refresh_token", tokens["refresh_token"].as_str().unwrap()),
            ("resource", "https://ssmcp.example/mcp"),
        ])
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn real_core_process_restarts_preserve_tokens_and_refresh_replay_revocation() {
    let directory = Directory::new();
    {
        let mut store = crate::portal::store::Store::open(&directory.0.join("accounts")).unwrap();
        store
            .login(&Identity {
                provider: "github".into(),
                subject: "123".into(),
                login: "alice".into(),
            })
            .unwrap();
        store.claim_handle("123", "alice").unwrap();
    }
    let tokens = {
        let oauth = directory.open().unwrap();
        oauth.test_identity_tokens("mcp", "123")
    };
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let (mut first, base) = start_process(&directory, 1).await;
    assert_eq!(
        initialize(&client, &base, tokens["access_token"].as_str().unwrap()).await,
        StatusCode::OK
    );
    let response = http_refresh(&client, &base, &tokens).await;
    assert_eq!(response.status(), StatusCode::OK);
    let mut rotated: Value = response.json().await.unwrap();
    rotated["client_id"] = tokens["client_id"].clone();
    first.stop();
    drop(first);
    let (mut second, base) = start_process(&directory, 2).await;
    assert_eq!(
        initialize(&client, &base, rotated["access_token"].as_str().unwrap()).await,
        StatusCode::OK
    );
    let replay = http_refresh(&client, &base, &tokens).await;
    assert_eq!(replay.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        replay.json::<Value>().await.unwrap()["error"],
        "invalid_grant"
    );
    second.stop();
    drop(second);
    let (_third, base) = start_process(&directory, 3).await;
    assert_eq!(
        initialize(&client, &base, rotated["access_token"].as_str().unwrap()).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        http_refresh(&client, &base, &rotated).await.status(),
        StatusCode::BAD_REQUEST
    );
}

#[test]
#[ignore = "private child process entry; invoked only by the restart regression"]
fn grant_process_fixture() {
    let directory = PathBuf::from(
        std::env::var_os("SSMCP_GRANT_PROCESS_DIRECTORY").expect("private fixture directory"),
    );
    let address = PathBuf::from(
        std::env::var_os("SSMCP_GRANT_PROCESS_ADDRESS").expect("private fixture address"),
    );
    assert!(address.starts_with(&directory));
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let mut broker = Broker::new(ISSUER.into());
            broker
                .oauth
                .enable_client_registry(&directory.join("clients.toml"))
                .unwrap();
            broker.portal = crate::portal::Portal::open(&directory.join("accounts")).unwrap();
            broker
                .configure_user_policies(crate::user_policy::DEFAULT_RATE_LIMIT_BPS)
                .await
                .unwrap();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let mut file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .open(address.with_extension("tmp"))
                .unwrap();
            file.write_all(&serde_json::to_vec(&json!({"base":base})).unwrap())
                .unwrap();
            drop(file);
            fs::rename(address.with_extension("tmp"), &address).unwrap();
            axum::serve(listener, crate::http::router(Arc::new(broker)))
                .await
                .unwrap();
        });
}
