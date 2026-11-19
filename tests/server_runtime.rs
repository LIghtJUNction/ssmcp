//! Runtime preflight must not load credentials or silently accept serve options.
#![cfg(target_os = "linux")]

use std::process::Command;

fn command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ssmcpd"));
    command
        .env("SSMCP_CONFIG", "/ssmcp-runtime-test-missing-config.toml")
        .env("SSMCP_PUBLIC_URL", "invalid origin")
        .env("SSMCP_ACCESS_TOKEN_TTL", "invalid duration");
    command
}

#[test]
fn runtime_help_is_available_without_configuration_or_dependencies() {
    for args in [["check-runtime", "--help"], ["check-runtime", "-h"]] {
        let result = command().env("PATH", "").args(args).output().unwrap();
        assert!(result.status.success());
        assert!(result.stderr.is_empty());
        let text = String::from_utf8(result.stdout).unwrap();
        assert!(text.contains("ssmcpd check-runtime"));
        assert!(text.contains("mandatory OS sandbox"));
    }
}

#[test]
fn runtime_check_rejects_options_before_discovering_the_runtime() {
    for args in [
        vec!["--config", "/some/config.toml"],
        vec!["--socket", "/some/control.sock"],
        vec!["--portal-only"],
        vec!["--help", "unexpected"],
        vec!["unexpected"],
    ] {
        let result = command()
            .env("PATH", "")
            .arg("check-runtime")
            .args(args)
            .output()
            .unwrap();
        assert_eq!(result.status.code(), Some(1));
        assert!(result.stdout.is_empty());
        assert_eq!(
            String::from_utf8(result.stderr).unwrap(),
            "ssmcpd: check-runtime takes no options or arguments; see --help\n"
        );
    }
}

#[test]
fn runtime_check_reports_the_dependency_failure_without_loading_server_configuration() {
    let result = command()
        .env("PATH", "")
        .arg("check-runtime")
        .output()
        .unwrap();
    assert_eq!(result.status.code(), Some(1));
    assert!(result.stdout.is_empty());
    assert_eq!(
        String::from_utf8(result.stderr).unwrap(),
        "ssmcpd: node is not installed in PATH\n"
    );
}

#[test]
#[ignore = "requires system Node.js and bubblewrap namespace support"]
fn runtime_check_succeeds_without_gateway_configuration() {
    let result = command()
        .env_remove("SSMCP_CODE_MODE_RUNNER")
        .arg("check-runtime")
        .output()
        .unwrap();
    assert!(result.status.success(), "{:?}", result);
    assert!(result.stderr.is_empty());
    assert_eq!(result.stdout, b"{\"code_mode\":\"ok\"}\n");
}
