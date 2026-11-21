use super::*;
use crate::oauth::{Authorization, HeaderMap, Parameters};
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::fs::{DirBuilderExt, PermissionsExt, symlink},
};

const ISSUER: &str = "https://ssmcp.example";

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let name = super::super::random("ssmcp-oauth-transfer-").ok().unwrap();
        let path = std::env::temp_dir().join(name);
        fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        fs::DirBuilder::new()
            .mode(0o700)
            .create(path.join("source"))
            .unwrap();
        Self(path)
    }
    fn registry(&self) -> PathBuf {
        self.0.join("source").join(CLIENTS)
    }
    fn oauth(&self) -> OAuth {
        let mut oauth = OAuth::new(ISSUER.into());
        oauth.enable_client_registry(&self.registry()).unwrap();
        oauth
    }
    fn args(&self, action: &str, source: &Path, output: &Path) -> Vec<String> {
        [
            action,
            "--public-url",
            ISSUER,
            if action == "export" {
                "--client-registry"
            } else {
                "--input"
            },
            source.to_str().unwrap(),
            "--output",
            output.to_str().unwrap(),
        ]
        .into_iter()
        .map(str::to_owned)
        .collect()
    }
    fn export(&self, output: &Path) -> Value {
        serde_json::from_str(&run(&self.args("export", &self.registry(), output)).unwrap()).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn refresh(oauth: &OAuth, tokens: &Value) -> super::super::OResult<Value> {
    oauth.token(
        &Parameters(
            [
                ("grant_type", "refresh_token"),
                ("client_id", tokens["client_id"].as_str().unwrap()),
                ("refresh_token", tokens["refresh_token"].as_str().unwrap()),
                ("resource", "https://ssmcp.example/mcp"),
            ]
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value.to_owned()))
            .collect(),
        ),
        &HeaderMap::new(),
    )
}

fn subject(oauth: &OAuth, tokens: &Value) -> Option<String> {
    match oauth.authenticate("mcp", tokens["access_token"].as_str().unwrap())? {
        Authorization::Identity(identity) => Some(identity.subject),
        Authorization::Legacy { .. } => None,
    }
}

fn private(path: &Path) {
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}

#[test]
fn migration_round_trip_preserves_access_expiry_refresh_replay_and_revocation() {
    let fixture = Fixture::new();
    let oauth = fixture.oauth();
    let (alice, code) = oauth.test_identity_grant("mcp", "123");
    let bob = oauth.test_identity_tokens("mcp", "456");
    let rotated = refresh(&oauth, &alice).ok().unwrap();
    oauth.revoke_subject("456").unwrap();
    let absolute = oauth
        .state
        .lock()
        .unwrap()
        .families
        .values()
        .find(|family| {
            matches!(&family.binding.authorization,
            Authorization::Identity(identity) if identity.subject == "123")
        })
        .unwrap()
        .absolute_expires_at_ms;
    let code_key = super::super::hash(&code);
    let code_binding = {
        let state = oauth.state.lock().unwrap();
        let code = &state.codes[&code_key];
        (code.challenge, code.redirect_uri.clone(), code.used_family)
    };
    drop(oauth);

    let exported = fixture.0.join("exported");
    let receipt = fixture.export(&exported);
    assert_eq!(receipt["coverage"], "account_grants");
    for path in [
        exported.join(CLIENTS),
        exported.join(GRANTS),
        exported.join(MANIFEST),
    ] {
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        let text = fs::read_to_string(path).unwrap();
        for tokens in [&alice, &rotated, &bob] {
            assert!(!text.contains(tokens["access_token"].as_str().unwrap()));
            assert!(!text.contains(tokens["refresh_token"].as_str().unwrap()));
        }
    }
    assert_eq!(fs::metadata(&exported).unwrap().mode() & 0o777, 0o700);
    let restored = fixture.0.join("restored");
    let result: Value =
        serde_json::from_str(&run(&fixture.args("import", &exported, &restored)).unwrap()).unwrap();
    assert_eq!(result["status"], "imported");
    assert_eq!(result["configuration_changed"], false);
    let loaded = load(&restored.join(CLIENTS), ISSUER).unwrap();
    assert_eq!(subject(&loaded.oauth, &rotated).as_deref(), Some("123"));
    assert!(subject(&loaded.oauth, &bob).is_none());
    assert_eq!(
        loaded
            .oauth
            .state
            .lock()
            .unwrap()
            .families
            .values()
            .next()
            .unwrap()
            .absolute_expires_at_ms,
        absolute
    );
    {
        let state = loaded.oauth.state.lock().unwrap();
        let code = &state.codes[&code_key];
        assert_eq!(
            (code.challenge, code.redirect_uri.clone(), code.used_family),
            code_binding
        );
    }
    assert!(refresh(&loaded.oauth, &alice).is_err());
    assert!(subject(&loaded.oauth, &rotated).is_none());
    drop(loaded);
    assert!(
        subject(
            &load(&restored.join(CLIENTS), ISSUER).unwrap().oauth,
            &rotated
        )
        .is_none()
    );
}

#[test]
fn migration_rejects_live_source_and_missing_primary_without_creating_an_export() {
    let fixture = Fixture::new();
    let oauth = fixture.oauth();
    let output = fixture.0.join("exported");
    assert!(run(&fixture.args("export", &fixture.registry(), &output)).is_err());
    assert!(!output.exists());
    drop(oauth);
    let missing = fixture.0.join("source/missing.toml");
    assert!(run(&fixture.args("export", &missing, &output)).is_err());
    assert!(!missing.exists());
    assert!(!fixture.0.join("source/missing.toml.lock").exists());
    assert!(!output.exists());
}

#[test]
fn migration_marks_missing_account_grants_as_dcr_only_and_keeps_pending_intents() {
    let fixture = Fixture::new();
    drop(fixture.oauth());
    let output = fixture.0.join("dcr-only");
    let result = fixture.export(&output);
    assert_eq!(result["coverage"], "client_registrations_only");
    assert_eq!(result["restored_account_families"], 0);
    let imported = fixture.0.join("dcr-imported");
    let result: Value =
        serde_json::from_str(&run(&fixture.args("import", &output, &imported)).unwrap()).unwrap();
    assert_eq!(result["coverage"], "client_registrations_only");

    let pending = fixture
        .0
        .join("source/oauth-clients.toml.grants.json.pending");
    fs::write(&pending, b"unfinished").unwrap();
    private(&pending);
    let blocked = fixture.0.join("blocked");
    assert!(run(&fixture.args("export", &fixture.registry(), &blocked)).is_err());
    assert_eq!(fs::read(&pending).unwrap(), b"unfinished");
    assert!(!blocked.exists());
}

#[test]
fn import_rejects_wrong_issuer_modified_members_and_unsafe_permissions_before_publication() {
    let fixture = Fixture::new();
    let oauth = fixture.oauth();
    oauth.test_identity_tokens("mcp", "123");
    drop(oauth);
    let package = fixture.0.join("package");
    fixture.export(&package);
    let output = fixture.0.join("restored");
    let mut args = fixture.args("import", &package, &output);
    args[2] = "https://other.example".into();
    assert!(run(&args).is_err());
    assert!(!output.exists());

    let grants = package.join(GRANTS);
    let original = fs::read(&grants).unwrap();
    let mut changed = original.clone();
    changed.push(b' ');
    fs::write(&grants, &changed).unwrap();
    assert!(run(&fixture.args("import", &package, &output)).is_err());
    assert!(!output.exists());
    fs::write(&grants, &original).unwrap();
    fs::set_permissions(&grants, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(run(&fixture.args("import", &package, &output)).is_err());
    assert!(!output.exists());
    private(&grants);
    let linked = fixture.0.join("grants-hardlink");
    fs::hard_link(&grants, &linked).unwrap();
    assert!(run(&fixture.args("import", &package, &output)).is_err());
    fs::remove_file(linked).unwrap();

    let symlink_package = fixture.0.join("package-link");
    symlink(&package, &symlink_package).unwrap();
    assert!(run(&fixture.args("import", &symlink_package, &output)).is_err());
    assert!(!output.exists());
}

#[test]
fn migration_never_replaces_an_existing_or_racing_destination() {
    let fixture = Fixture::new();
    drop(fixture.oauth());
    let target = fixture.0.join("existing");
    fs::create_dir(&target).unwrap();
    fs::write(target.join("keep"), b"unchanged").unwrap();
    assert!(run(&fixture.args("export", &fixture.registry(), &target)).is_err());
    assert_eq!(fs::read(target.join("keep")).unwrap(), b"unchanged");
    let target = fixture.0.join("raced");
    let mut stage = Stage::new(&target).unwrap();
    fs::create_dir(&target).unwrap();
    fs::write(target.join("keep"), b"racing owner").unwrap();
    assert!(stage.publish().is_err());
    drop(stage);
    assert_eq!(fs::read(target.join("keep")).unwrap(), b"racing owner");
    assert!(!fs::read_dir(&fixture.0).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".oauth-migration-")
    }));
}

#[test]
fn migration_help_and_argument_errors_do_not_touch_paths_or_initialize_a_runtime() {
    for args in [
        vec![],
        vec!["--help"],
        vec!["export", "--help"],
        vec!["import", "-h"],
    ] {
        assert_eq!(
            run(&args.into_iter().map(str::to_owned).collect::<Vec<_>>()).unwrap(),
            HELP
        );
    }
    for args in [
        vec!["export"],
        vec![
            "export",
            "--public-url",
            "https://example",
            "--client-registry",
            "/missing",
            "--output",
            "/unused",
            "--output",
            "/other",
        ],
        vec![
            "import",
            "--public-url",
            "https://example",
            "--input",
            "relative",
            "--output",
            "/unused",
        ],
        vec![
            "import",
            "--public-url",
            "https://user:secret@example",
            "--input",
            "/missing",
            "--output",
            "/unused",
        ],
        vec![
            "import",
            "--public-url",
            "https://example",
            "--input",
            "/missing",
            "--client-registry",
            "/unexpected",
            "--output",
            "/unused",
        ],
    ] {
        assert!(parse(&args.into_iter().map(str::to_owned).collect::<Vec<_>>()).is_err());
    }
    assert_eq!(
        serde_json::to_value(Coverage::ClientRegistrationsOnly).unwrap(),
        json!("client_registrations_only")
    );
}
