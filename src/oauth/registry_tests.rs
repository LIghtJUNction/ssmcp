use super::*;
use std::{
    fs,
    os::unix::fs::{DirBuilderExt, PermissionsExt, symlink},
    path::PathBuf,
};

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let path = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(random("ssmcp-registry-test-").ok().unwrap());
        fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        Self(path)
    }
    fn file(&self) -> PathBuf {
        self.0.join("clients.toml")
    }
    fn oauth(&self) -> OAuth {
        let mut oauth = OAuth::new("https://ssmcp.example".into());
        oauth.enable_client_registry(&self.file()).unwrap();
        oauth
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn register(oauth: &OAuth, method: &str) -> Value {
    oauth
        .register(
            serde_json::from_value(json!({
                "redirect_uris":["https://client.example/callback"], "client_name":"Restart test",
                "token_endpoint_auth_method":method
            }))
            .unwrap(),
        )
        .ok()
        .expect("registration succeeds")
}
fn params(items: &[(&str, &str)]) -> Parameters {
    Parameters(
        items
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
    )
}
const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
const CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
fn authorize(oauth: &OAuth, id: &str) -> OResult<Consent> {
    oauth.begin_authorization(
        &params(&[
            ("client_id", id),
            ("redirect_uri", "https://client.example/callback"),
            ("response_type", "code"),
            ("resource", "https://ssmcp.example/alice"),
            ("code_challenge_method", "S256"),
            ("code_challenge", CHALLENGE),
        ]),
        &HeaderMap::new(),
    )
}

#[tokio::test]
async fn restart_restores_public_basic_and_post_clients_for_real_token_exchange() {
    for method in ["none", "client_secret_basic", "client_secret_post"] {
        let dir = Directory::new();
        let registered = {
            let oauth = dir.oauth();
            register(&oauth, method)
        };
        let id = registered["client_id"].as_str().unwrap();
        let secret = registered["client_secret"].as_str();
        let text = fs::read_to_string(dir.file()).unwrap();
        assert!(text.contains(id));
        if let Some(secret) = secret {
            assert!(!text.contains(secret));
        }
        let mut broker = Broker::new("https://ssmcp.example".into());
        broker.oauth.enable_client_registry(&dir.file()).unwrap();
        let form = authorize(&broker.oauth, id)
            .ok()
            .expect("same client ID survives restart");
        let session = broker.test_register("alice", &"a".repeat(64)).await;
        let attempt = broker.oauth.begin_login(&form.id, &form.csrf).ok().unwrap();
        let code = attempt.code.clone();
        assert!(broker.oauth.finish_login(attempt, session.clone()).is_ok());
        let mut p = params(&[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("code_verifier", VERIFIER),
            ("resource", "https://ssmcp.example/alice"),
        ]);
        let mut headers = HeaderMap::new();
        if method == "client_secret_basic" {
            headers.insert(
                header::AUTHORIZATION,
                format!(
                    "Basic {}",
                    STANDARD.encode(format!("{id}:{}", secret.unwrap()))
                )
                .parse()
                .unwrap(),
            );
        } else {
            p.0.insert("client_id".into(), id.into());
            if let Some(secret) = secret {
                p.0.insert("client_secret".into(), secret.into());
            }
        }
        let tokens = broker
            .oauth
            .token(&p, &headers)
            .ok()
            .expect("saved secret verifier remains valid");
        assert!(tokens["access_token"].as_str().unwrap().starts_with("at_"));
        assert!(tokens["refresh_token"].as_str().unwrap().starts_with("rt_"));
        assert!(authorize(&broker.oauth, "client_missing").is_err());
        drop(broker);
        let oauth = dir.oauth();
        assert!(
            oauth
                .authenticate("alice", tokens["access_token"].as_str().unwrap())
                .is_none()
        );
        assert!(authorize(&oauth, id).is_ok());
    }
}

#[test]
fn restart_does_not_extend_expiration_and_expired_records_are_dropped() {
    let dir = Directory::new();
    let oauth = dir.oauth();
    let registered = register(&oauth, "none");
    let id = registered["client_id"].as_str().unwrap();
    let expires = oauth.state.lock().unwrap().clients[id].expires_unix;
    drop(oauth);
    let oauth = dir.oauth();
    assert_eq!(
        oauth.state.lock().unwrap().clients[id].expires_unix,
        expires
    );
    drop(oauth);
    let text = fs::read_to_string(dir.file())
        .unwrap()
        .replace(&format!("expires_unix = {expires}"), "expires_unix = 1");
    fs::write(dir.file(), text).unwrap();
    let oauth = dir.oauth();
    assert!(authorize(&oauth, id).is_err());
}

#[test]
fn unsafe_permissions_symlinks_hardlinks_and_competing_writer_fail_closed() {
    let dir = Directory::new();
    let oauth = dir.oauth();
    let mut second = OAuth::new("https://ssmcp.example".into());
    assert!(second.enable_client_registry(&dir.file()).is_err());
    drop(oauth);
    fs::set_permissions(dir.file(), fs::Permissions::from_mode(0o644)).unwrap();
    assert!(second.enable_client_registry(&dir.file()).is_err());
    fs::set_permissions(dir.file(), fs::Permissions::from_mode(0o600)).unwrap();
    fs::rename(dir.file(), dir.0.join("original")).unwrap();
    symlink(dir.0.join("original"), dir.file()).unwrap();
    assert!(second.enable_client_registry(&dir.file()).is_err());
    fs::remove_file(dir.file()).unwrap();
    fs::hard_link(dir.0.join("original"), dir.file()).unwrap();
    assert!(second.enable_client_registry(&dir.file()).is_err());
    fs::remove_file(dir.0.join("original")).unwrap();
    fs::set_permissions(&dir.0, fs::Permissions::from_mode(0o750)).unwrap();
    assert!(second.enable_client_registry(&dir.file()).is_err());
}

#[test]
fn corrupt_registry_and_different_issuer_never_reset_the_file() {
    let dir = Directory::new();
    drop(dir.oauth());
    let mut other = OAuth::new("https://other.example".into());
    assert!(other.enable_client_registry(&dir.file()).is_err());
    let secret = "invalid-secret-marker = [\"must-not-leak";
    fs::write(dir.file(), secret).unwrap();
    let mut oauth = OAuth::new("https://ssmcp.example".into());
    let error = oauth
        .enable_client_registry(&dir.file())
        .err()
        .unwrap()
        .to_string();
    assert!(!error.contains("invalid-secret-marker") && !error.contains("must-not-leak"));
    assert_eq!(fs::read_to_string(dir.file()).unwrap(), secret);
}

#[test]
fn failed_save_never_acknowledges_an_ephemeral_registration() {
    let dir = Directory::new();
    let oauth = dir.oauth();
    register(&oauth, "none");
    let saved = fs::read(dir.file()).unwrap();
    fs::set_permissions(dir.file(), fs::Permissions::from_mode(0o400)).unwrap();
    let result = oauth.register(
        serde_json::from_value(json!({"redirect_uris":["https://client.example/callback"]}))
            .unwrap(),
    );
    assert!(result.is_err());
    assert_eq!(oauth.state.lock().unwrap().clients.len(), 1);
    assert_eq!(fs::read(dir.file()).unwrap(), saved);
    fs::set_permissions(dir.file(), fs::Permissions::from_mode(0o600)).unwrap();
}

#[test]
fn concurrent_registrations_survive_reload_without_lost_updates() {
    let dir = Directory::new();
    let oauth = Arc::new(dir.oauth());
    let tasks: Vec<_> = (0..16)
        .map(|_| {
            let oauth = oauth.clone();
            std::thread::spawn(move || register(&oauth, "none"))
        })
        .collect();
    let ids: Vec<_> = tasks
        .into_iter()
        .map(|t| t.join().unwrap()["client_id"].as_str().unwrap().to_owned())
        .collect();
    drop(oauth);
    let oauth = dir.oauth();
    for id in ids {
        assert!(authorize(&oauth, &id).is_ok());
    }
    assert_eq!(oauth.state.lock().unwrap().clients.len(), 16);
}

#[test]
fn native_mixed_callbacks_survive_and_invalid_records_are_rejected() {
    let dir = Directory::new();
    let oauth = dir.oauth();
    let registration = serde_json::from_value(json!({"application_type":"native", "redirect_uris":["http://127.0.0.1:48152/callback","https://client.example/callback"],"token_endpoint_auth_method":"none"})).unwrap();
    assert!(oauth.register(registration).is_ok());
    drop(oauth);
    drop(dir.oauth());
    let original = fs::read_to_string(dir.file()).unwrap();
    for invalid in [
        original.replace("version = 1", "version = 99"),
        original.replace("method = \"none\"", "method = \"client_secret_basic\""),
        original.replace(
            "application_type = \"native\"",
            "application_type = \"web\"",
        ),
    ] {
        fs::write(dir.file(), invalid).unwrap();
        let mut oauth = OAuth::new("https://ssmcp.example".into());
        assert!(oauth.enable_client_registry(&dir.file()).is_err());
    }
}
