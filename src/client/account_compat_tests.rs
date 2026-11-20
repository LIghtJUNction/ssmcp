//! Explicit compatibility policy and durable CLI settings.
use super::*;

fn options(args: &[&str]) -> Options {
    Options::parse(args.iter().map(OsString::from))
        .unwrap()
        .unwrap()
}

#[test]
fn account_compat_is_explicit_and_preserves_allow_in_saved_sessions() {
    let mut selected = options(&[
        "--server",
        "https://relay.example",
        "--account-compat",
        "--allow",
        "alice@relay.example",
    ]);
    assert!(selected.account_compat);
    assert!(selected.registration_metadata().is_err());
    selected.relay = Some(account::Credential {
        server: "https://relay.example".into(),
        token: "a".repeat(64),
        principal: "octocat@github".into(),
        subject: Some("123".into()),
        username: Some("alice".into()),
        expires_at: u64::MAX,
    });
    assert!(selected.registration_metadata().unwrap().allow.is_empty());
    assert_eq!(selected.metadata().unwrap().allow, ["alice@relay.example"]);
    let manifest = selected.session_spec().unwrap();
    assert!(manifest.args.iter().any(|arg| arg == "--account-compat"));
    assert!(
        !manifest
            .args
            .iter()
            .any(|arg| arg.contains(&"a".repeat(64)))
    );
    let restored = Options::parse_saved(&manifest).unwrap().unwrap();
    assert!(restored.account_compat);
    assert_eq!(restored.allow, selected.allow);
    assert!(restored.relay.is_none());
    assert!(restored.registration_metadata().is_err());

    let anonymous = options(&[
        "--server",
        "https://relay.example",
        "--allow",
        "alice@relay.example",
    ]);
    assert!(!anonymous.account_compat);
    assert!(anonymous.relay.is_none());
    assert_eq!(
        anonymous.registration_metadata().unwrap().allow,
        anonymous.allow
    );
}

#[test]
fn account_compat_rejects_ambiguous_modes_and_unverified_wire_bindings() {
    for args in [
        vec!["--account-compat"],
        vec![
            "--account-compat",
            "--allow",
            "alice@relay.example,bob@relay.example",
        ],
        vec![
            "--account-compat",
            "--allow",
            "alice@relay.example",
            "alice@relay.example",
        ],
        vec![
            "--account-compat",
            "--allow",
            "alice@relay.example",
            "--username",
            "alice",
        ],
        vec![
            "--account-compat",
            "--allow",
            "alice@relay.example",
            "--list-tools",
        ],
        vec![
            "local",
            "--account-compat",
            "--allow",
            "alice@relay.example",
        ],
    ] {
        assert!(Options::parse(args.into_iter().map(OsString::from)).is_err());
    }
    let mut selected = options(&[
        "--server",
        "https://relay.example",
        "--account-compat",
        "--allow",
        "alice@relay.example",
    ]);
    for (server, subject, username) in [
        ("https://other.example", Some("123"), Some("alice")),
        ("https://relay.example", None, Some("alice")),
        ("https://relay.example", Some("123"), None),
        ("https://relay.example", Some("123"), Some("bob")),
    ] {
        selected.relay = Some(account::Credential {
            server: server.into(),
            token: "a".repeat(64),
            principal: "octocat@github".into(),
            subject: subject.map(str::to_owned),
            username: username.map(str::to_owned),
            expires_at: u64::MAX,
        });
        assert!(selected.registration_metadata().is_err());
    }
}

#[test]
fn account_compat_relay_omits_delegation_and_keeps_credentials_out_of_arguments() {
    let credential = account::Credential {
        server: "https://relay.example".into(),
        token: "a".repeat(64),
        principal: "octocat@github".into(),
        subject: Some("123".into()),
        username: Some("alice".into()),
        expires_at: u64::MAX,
    };
    let mut command = relay::command(
        std::path::Path::new("ssmcp"),
        &credential.server,
        Some(&credential),
    );
    relay::restrict_to_self(&mut command);
    let command = command.as_std();
    let offer = command
        .get_envs()
        .find(|(name, _)| *name == "SSMCP_RELAY_CAPABILITIES")
        .and_then(|(_, value)| value)
        .unwrap();
    assert_eq!(offer, crate::wire::CONTROL_ATTRIBUTION_CAPABILITY);
    assert!(
        !command
            .get_args()
            .any(|arg| arg.to_string_lossy().contains(&credential.token))
    );
}
