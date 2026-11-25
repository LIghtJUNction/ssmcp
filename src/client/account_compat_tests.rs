//! Device startup rejects a saved account-authenticated transport definition.
use super::*;

#[test]
fn new_and_saved_connections_reject_account_compat() {
    let args = [
        "--server",
        "https://relay.example",
        "--allow",
        "alice@relay.example",
    ];
    let options = Options::parse(args.map(OsString::from)).unwrap().unwrap();
    let mut spec = options.session_spec().unwrap();
    spec.args.push("--account-compat".into());
    let error = Options::parse_saved(&spec).err().unwrap();
    assert!(error.contains("--account-compat was removed"), "{error}");

    let error = Options::parse_new(["--account-compat"].map(OsString::from))
        .err()
        .unwrap();
    assert!(error.contains("--account-compat was removed"), "{error}");
}

#[test]
fn transport_child_does_not_inherit_account_bearer() {
    let command = relay::command(std::path::Path::new("ssmcp"), "https://relay.example");
    assert!(
        command
            .as_std()
            .get_envs()
            .any(|(name, value)| name == "SSMCP_RELAY_TOKEN" && value.is_none())
    );
}
