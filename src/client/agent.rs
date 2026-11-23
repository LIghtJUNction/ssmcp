//! Explicit startup selection over the same lazy harness registry as tool calls.
use crate::{
    backend::peer,
    registry::{OPTIONAL_PROVIDERS, Provider},
    runtime::Runtime,
    wire::Operation,
};
use serde_json::json;
use std::{ffi::OsString, sync::Arc};

pub(crate) const HELP: &str = "\
ssmcp [connection options] --load-harness NAME [-- native protocol arguments]
ssmcp --list-harnesses [--json]

ssmcp                     Explicit authenticated MCP peer; no child ssmcp is started

Third-party harness import has been removed. Run that tool externally, or use
SSMCP terminal or exec for explicit commands. Existing saved files are retained.
Peer arguments after --: --endpoint URL --token-env NAME [--socket /absolute/path]
Use the peer's exact displayed MCP URL, including /USER; this names a gateway,
not an individual device/session. The environment variable contains its OAuth bearer.
Unix socket transport keeps that URL's Host, Origin and OAuth checks.
";

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Selection {
    pub provider: Provider,
    pub arguments: Vec<OsString>,
    pub peer: Option<peer::Settings>,
}

impl Selection {
    pub fn new(name: &str, arguments: Vec<OsString>) -> Result<Self, String> {
        if matches!(
            name,
            "codex" | "claude" | "claude-code" | "cc" | "pi" | "opencode"
        ) {
            return Err("built-in third-party harness import was removed; run it externally or use explicit terminal/exec. Existing session files are retained".into());
        }
        let provider = OPTIONAL_PROVIDERS
            .iter()
            .find(|(_, canonical)| *canonical == name)
            .map(|(provider, _)| *provider)
            .ok_or("unknown --load-harness; use ssmcp --list-harnesses")?;
        if arguments.len() > 128 || arguments.iter().map(|arg| arg.len()).sum::<usize>() > 16 * 1024
        {
            return Err("agent arguments exceed 128 arguments or 16384 bytes".into());
        }
        for argument in &arguments {
            let text = argument.to_str().ok_or("agent arguments must be UTF-8")?;
            if text.contains('\0') {
                return Err("agent arguments cannot contain NUL".into());
            }
        }
        let peer = Some(peer::Settings::parse(&arguments)?);
        Ok(Self {
            provider,
            arguments,
            peer,
        })
    }

    pub fn check_target(&self, own_endpoint: Option<&str>) -> Result<(), String> {
        if let (Some(peer), Some(own)) = (&self.peer, own_endpoint) {
            peer.reject_self(own)?;
        }
        Ok(())
    }

    pub fn append_saved(&self, args: &mut Vec<OsString>) {
        args.extend([
            OsString::from("--load-harness"),
            OsString::from(self.provider.label()),
        ]);
        if !self.arguments.is_empty() {
            args.push(OsString::from("--"));
            args.extend(self.arguments.iter().cloned());
        }
    }

    pub async fn activate(&self, runtime: &Arc<Runtime>) -> Result<(), String> {
        let result = runtime
            .perform(Operation::CallTool {
                name: "ssmcp.harnesses.load".into(),
                arguments: json!({"provider":self.provider.label()}),
            })
            .await?;
        if result["isError"] == true || result["structuredContent"]["status"] != "ready" {
            return Err(format!(
                "selected harness {} did not finish native protocol initialization",
                self.provider.label()
            ));
        }
        Ok(())
    }
}

/// This exact, local information command runs before config/account resolution.
/// It never looks for executables, credentials, contexts, or a running peer.
pub(super) fn dispatch(args: &[OsString]) -> Result<bool, String> {
    let Some((first, tail)) = args.split_first() else {
        return Ok(false);
    };
    if first != "--list-harnesses" {
        return Ok(false);
    }
    if tail.is_empty() {
        print!("{HELP}");
    } else if tail == [OsString::from("--json")] {
        let providers = OPTIONAL_PROVIDERS
            .iter()
            .map(|(_, name)| {
                json!({
                    "name":name,
                    "aliases":[],
                    "process_arguments":false,
                    "model_turn_started":false,
                    "installation":"not_probed",
                })
            })
            .collect::<Vec<_>>();
        println!("{}", json!({"providers":providers,"help":HELP}));
    } else {
        return Err("ssmcp --list-harnesses accepts only --json".into());
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::super::Options;
    use super::*;

    fn options(args: &[&str]) -> Options {
        Options::parse(args.iter().map(OsString::from))
            .unwrap()
            .unwrap()
    }

    #[test]
    fn removed_harnesses_have_an_explicit_migration_error() {
        for alias in ["codex", "claude", "claude-code", "cc", "pi", "opencode"] {
            assert!(
                Selection::new(alias, vec![])
                    .unwrap_err()
                    .contains("third-party harness import was removed")
            );
        }
        assert_eq!(OPTIONAL_PROVIDERS, &[(Provider::Peer, "ssmcp")]);
        assert!(options(&["--list-tools"]).harness.is_none());
    }

    #[test]
    fn harness_cli_rejects_ambiguous_or_unsupported_selection() {
        for args in [
            vec!["--list-tools", "--load-harness"],
            vec!["--list-tools", "--load-harness", "native"],
            vec![
                "--list-tools",
                "--load-harness",
                "codex",
                "--load-harness=claude",
            ],
            vec!["--list-tools", "--", "--help"],
            vec!["--list-tools", "--agent", "list"],
            vec![
                "--server",
                "https://relay.example",
                "--agent=codex",
                "--",
                "--help",
            ],
            vec![
                "--server",
                "https://relay.example",
                "--agent",
                "next-caller",
                "--agent=codex",
            ],
            vec![
                "--list-tools",
                "--load-harness",
                "pi",
                "--",
                "--model",
                "test",
            ],
            vec!["--list-tools", "--load-harness", "opencode", "--", "serve"],
            vec!["--list-tools", "--load-harness", "playwright"],
            vec![
                "--list-tools",
                "--load-harness",
                "playwright",
                "--",
                "--headless",
            ],
            vec!["--list-tools", "--load-harness", "ssmcp"],
            vec!["--list-tools", "--type", "codex"],
        ] {
            assert!(
                Options::parse(args.iter().map(OsString::from)).is_err(),
                "{args:?}"
            );
        }
    }

    #[test]
    fn saved_peer_arguments_roundtrip_with_native_session_metadata() {
        let arguments = [
            "--endpoint",
            "https://peer.example/mcp/test",
            "--token-env",
            "SSMCP_TEST_PEER_TOKEN",
        ];
        let parsed = options(&[
            "--server",
            "https://relay.example",
            "--name",
            "selected",
            "--title",
            "session title",
            "--input",
            "none",
            "--load-harness",
            "ssmcp",
            "--",
            arguments[0],
            arguments[1],
            arguments[2],
            arguments[3],
        ]);
        let spec = parsed.session_spec().unwrap();
        super::super::validate_session_spec(&spec).unwrap();
        assert!(!super::super::saved_chat_enabled(&spec));
        let restored = Options::parse_saved(&spec).unwrap().unwrap();
        assert_eq!(restored.title.as_deref(), Some("session title"));
        assert_eq!(restored.harness, parsed.harness);
        assert_eq!(
            restored.harness.unwrap().arguments,
            arguments.map(OsString::from)
        );
    }

    #[test]
    fn saved_external_harness_requires_migration_without_changing_saved_arguments() {
        let parsed = options(&["--server", "https://relay.example", "--name", "legacy"]);
        let mut spec = parsed.session_spec().unwrap();
        spec.args
            .extend(["--load-harness", "codex", "--", "--help"].map(String::from));
        let original = spec.args.clone();
        let error = super::super::validate_session_spec(&spec).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("third-party harness import was removed")
        );
        assert_eq!(spec.args, original);
    }
}
