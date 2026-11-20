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
ssmcp [connection options] --agent NAME [-- native protocol arguments]
ssmcp --agent list [--json]

codex                     Codex App Server (installed codex executable)
claude, claude-code, cc    Claude Code MCP server (installed claude executable)
pi                        Existing private SSMCP_PI_RPC_SOCKET session
opencode                  Existing SSMCP_OPENCODE_URL + OPENCODE_PASSWORD service
playwright                Existing Playwright MCP extension provider
ssmcp                     Explicit authenticated MCP peer; no child ssmcp is started

Only Codex/Claude accept native process arguments, and only after --.
Selection loads a protocol/catalog. It does not send a prompt or start a model turn.
Other providers keep their existing explicit attachment configuration.
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
        let name = match name {
            "claude-code" | "cc" => "claude",
            value => value,
        };
        let provider = OPTIONAL_PROVIDERS
            .iter()
            .find(|(_, canonical)| *canonical == name)
            .map(|(provider, _)| *provider)
            .ok_or("unknown --agent; use ssmcp --agent list")?;
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
        let peer = if provider == Provider::Peer {
            Some(peer::Settings::parse(&arguments)?)
        } else {
            if !arguments.is_empty() && !matches!(provider, Provider::Codex | Provider::Claude) {
                return Err(format!(
                    "--agent {name} uses its existing attachment configuration and does not accept process arguments after --; see ssmcp --agent list"
                ));
            }
            None
        };
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
            OsString::from("--agent"),
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
                "selected agent {} did not finish native protocol initialization",
                self.provider.label()
            ));
        }
        Ok(())
    }
}

/// This exact, local information command runs before config/account resolution.
/// It never looks for executables, credentials, contexts, or a running peer.
pub(super) fn dispatch(args: &[OsString]) -> Result<bool, String> {
    let tail = if args.first().is_some_and(|arg| arg == "--agent")
        && args.get(1).is_some_and(|arg| arg == "list")
    {
        &args[2..]
    } else if args.first().is_some_and(|arg| arg == "--agent=list") {
        &args[1..]
    } else {
        return Ok(false);
    };
    if tail.is_empty() {
        print!("{HELP}");
    } else if tail == [OsString::from("--json")] {
        let providers = OPTIONAL_PROVIDERS
            .iter()
            .map(|(_, name)| {
                json!({
                    "name":name,
                    "aliases":if *name == "claude" { vec!["claude-code", "cc"] } else { vec![] },
                    "process_arguments":matches!(*name, "codex" | "claude"),
                    "model_turn_started":false,
                    "installation":"not_probed",
                })
            })
            .collect::<Vec<_>>();
        println!("{}", json!({"providers":providers,"help":HELP}));
    } else {
        return Err("ssmcp --agent list accepts only --json".into());
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
    fn agent_cli_aliases_and_separator_preserve_native_arguments() {
        for alias in ["claude", "claude-code", "cc"] {
            let selected = options(&[
                "--list-tools",
                "--agent",
                alias,
                "--",
                "--help",
                "--name",
                "中文",
            ]);
            let selection = selected.agent.unwrap();
            assert_eq!(selection.provider, Provider::Claude);
            assert_eq!(
                selection.arguments,
                ["--help", "--name", "中文"].map(OsString::from)
            );
            assert!(selected.name.is_none());
        }
        assert_eq!(
            options(&["--list-tools", "--agent=codex"])
                .agent
                .unwrap()
                .provider,
            Provider::Codex
        );
        assert_eq!(
            options(&["--list-tools", "--title", "--agent"])
                .title
                .as_deref(),
            Some("--agent")
        );
    }

    #[test]
    fn agent_cli_rejects_ambiguous_or_unsupported_selection() {
        for args in [
            vec!["--list-tools", "--agent"],
            vec!["--list-tools", "--agent", "native"],
            vec!["--list-tools", "--agent", "codex", "--agent=claude"],
            vec!["--list-tools", "--", "--help"],
            vec!["--list-tools", "--agent", "pi", "--", "--model", "test"],
            vec!["--list-tools", "--agent", "opencode", "--", "serve"],
            vec!["--list-tools", "--agent", "playwright", "--", "--headless"],
            vec!["--list-tools", "--agent", "ssmcp"],
            vec!["--list-tools", "--type", "codex"],
        ] {
            assert!(
                Options::parse(args.iter().map(OsString::from)).is_err(),
                "{args:?}"
            );
        }
    }

    #[test]
    fn agent_saved_session_native_flags_do_not_change_metadata_or_validation() {
        assert!(super::super::connection_option_takes_value("--agent"));
        let parsed = options(&[
            "--server",
            "https://relay.example",
            "--name",
            "selected",
            "--title",
            "session title",
            "--agent",
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
        assert_eq!(parsed.agent, restored.agent);
        assert_eq!(restored.title.as_deref(), Some("session title"));
        let mut invalid = spec.clone();
        invalid.args.insert(0, "--help".into());
        assert!(super::super::validate_session_spec(&invalid).is_err());
    }

    #[test]
    fn agent_session_roundtrip_keeps_selection_and_native_separator() {
        let parsed = options(&[
            "--server",
            "https://relay.example",
            "--agent",
            "claude-code",
            "--",
            "--model",
            "fixture",
            "--name",
        ]);
        let spec = parsed.session_spec().unwrap();
        let restored = Options::parse_saved(&spec).unwrap().unwrap();
        assert_eq!(parsed.agent, restored.agent);
        assert_eq!(restored.server.as_deref(), Some("https://relay.example"));
        assert!(
            spec.args
                .windows(2)
                .any(|args| args == ["--agent", "claude"])
        );
        assert!(
            spec.args
                .ends_with(&["--", "--model", "fixture", "--name"].map(String::from))
        );
    }
}
