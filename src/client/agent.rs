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
    fn harness_cli_aliases_and_separator_preserve_native_arguments() {
        for alias in ["claude", "claude-code", "cc"] {
            let selected = options(&[
                "--list-tools",
                "--load-harness",
                alias,
                "--",
                "--help",
                "--name",
                "中文",
                "",
            ]);
            let selection = selected.harness.unwrap();
            assert_eq!(selection.provider, Provider::Claude);
            assert_eq!(
                selection.arguments,
                ["--help", "--name", "中文", ""].map(OsString::from)
            );
            assert!(selected.name.is_none());
        }
        assert_eq!(
            options(&["--list-tools", "--load-harness=codex"])
                .harness
                .unwrap()
                .provider,
            Provider::Codex
        );
        assert_eq!(
            options(&["--list-tools", "--title", "--load-harness"])
                .title
                .as_deref(),
            Some("--load-harness")
        );
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
    fn harness_saved_session_native_flags_do_not_change_metadata_or_validation() {
        assert!(super::super::connection_option_takes_value(
            "--load-harness"
        ));
        let parsed = options(&[
            "--server",
            "https://relay.example",
            "--name",
            "selected",
            "--title",
            "session title",
            "--load-harness",
            "codex",
            "--",
            "--help",
            "--version",
            "--title",
            "native title",
            "--context",
            "native-context",
        ]);
        let spec = parsed.session_spec().unwrap();
        super::super::validate_session_spec(&spec).unwrap();
        assert_eq!(
            super::super::session_title(&spec).unwrap().as_deref(),
            Some("session title")
        );
        let restored = Options::parse_saved(&spec).unwrap().unwrap();
        assert_eq!(parsed.harness, restored.harness);
        assert_eq!(restored.title.as_deref(), Some("session title"));
        let mut invalid = spec.clone();
        invalid.args.insert(0, "--help".into());
        assert!(super::super::validate_session_spec(&invalid).is_err());
    }

    #[test]
    fn legacy_saved_harness_needs_explicit_migration_with_or_without_native_tail() {
        let parsed = options(&["--server", "https://relay.example", "--name", "legacy"]);
        for tail in [vec![], vec!["--", "--title", "native title", ""]] {
            let mut spec = parsed.session_spec().unwrap();
            spec.args.extend(
                ["--agent", "codex"]
                    .into_iter()
                    .chain(tail)
                    .map(String::from),
            );
            let error = super::super::validate_session_spec(&spec).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("recreate the session with --load-harness")
            );
        }
    }

    #[test]
    fn disabled_chat_and_protocol_startup_roundtrip_independently() {
        let parsed = options(&[
            "--server",
            "https://relay.example",
            "--name",
            "quiet",
            "--load-harness",
            "codex",
            "--input",
            "none",
            "--agent=next-caller",
            "--",
            "--agent",
            "",
            "中文",
        ]);
        let spec = parsed.session_spec().unwrap();
        super::super::validate_session_spec(&spec).unwrap();
        assert!(!super::super::saved_chat_enabled(&spec));
        let restored = Options::parse_saved(&spec).unwrap().unwrap();
        assert_eq!(restored.chat, parsed.chat);
        assert_eq!(restored.harness, parsed.harness);
        assert_eq!(
            restored.harness.unwrap().arguments,
            ["--agent", "", "中文"].map(OsString::from)
        );
    }

    #[test]
    fn harness_session_roundtrip_keeps_selection_and_native_separator() {
        let token = std::env::temp_dir().join("chat token");
        let parsed = options(&[
            "--server",
            "https://relay.example",
            "--input",
            "mcp-http",
            "--input-host",
            "127.0.0.1",
            "--input-port",
            "8181",
            "--input-token-file",
            token.to_str().unwrap(),
            "--agent=codex",
            "--load-harness",
            "claude-code",
            "--",
            "--model",
            "fixture",
            "--name",
        ]);
        let spec = parsed.session_spec().unwrap();
        let restored = Options::parse_saved(&spec).unwrap().unwrap();
        assert_eq!(parsed.harness, restored.harness);
        assert_eq!(restored.server.as_deref(), Some("https://relay.example"));
        assert_eq!(restored.chat, parsed.chat);
        assert_eq!(restored.chat.agent, super::super::chat_io::Agent::Codex);
        super::super::validate_session_spec(&spec).unwrap();
        assert!(
            spec.args
                .windows(2)
                .any(|args| args == ["--load-harness", "claude"])
        );
        assert!(
            spec.args
                .ends_with(&["--", "--model", "fixture", "--name"].map(String::from))
        );
    }
}
