use std::{
    collections::BTreeMap,
    ffi::OsString,
    net::{IpAddr, SocketAddr},
    path::PathBuf,
};

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) enum Input {
    #[default]
    Terminal,
    None,
    Pipe,
    McpStdio,
    Web(Http),
    McpHttp(Http),
    Socket(Option<PathBuf>),
}

impl Input {
    pub(crate) fn uses_stdio(&self) -> bool {
        matches!(self, Self::Pipe | Self::McpStdio)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Http {
    pub(super) address: SocketAddr,
    pub(super) token_file: PathBuf,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum Agent {
    #[default]
    NextCaller,
    Codex,
    Claude,
    OpenCode,
    Exec {
        program: PathBuf,
        argv_file: Option<PathBuf>,
    },
    Ssmcp {
        id: String,
        account: (String, String),
    },
}

impl Agent {
    pub(super) fn label(&self) -> Option<&'static str> {
        Some(match self {
            Self::NextCaller => return None,
            Self::Codex => "Codex",
            Self::Claude => "Claude Code",
            Self::OpenCode => "OpenCode",
            Self::Exec { .. } => "Configured executable",
            Self::Ssmcp { .. } => "Configured SSMCP peer",
        })
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Settings {
    pub(crate) input: Input,
    pub(crate) agent: Agent,
    arguments: Vec<OsString>,
}

impl Settings {
    pub(crate) fn arguments(&self) -> &[OsString] {
        &self.arguments
    }

    pub(crate) fn validate_saved_input(&self) -> Result<(), String> {
        if self.input.uses_stdio()
            || (self.input == Input::Terminal && self.agent != Agent::NextCaller)
        {
            return Err("use web, MCP HTTP or sock for an explicit chat input or agent in a saved worker; terminal agent dispatch and standard streams require the foreground".into());
        }
        Ok(())
    }
}

const FLAGS: &[&str] = &[
    "--input",
    "--input-host",
    "--input-port",
    "--input-token-file",
    "--input-socket",
    "--agent",
    "--agent-peer",
    "--agent-account",
    "--agent-exec",
    "--agent-argv-file",
];

pub(crate) fn takes_value(flag: &str) -> bool {
    FLAGS.contains(&flag)
}

#[derive(Default)]
pub(crate) struct Flags {
    values: BTreeMap<String, String>,
    arguments: Vec<OsString>,
}

impl Flags {
    pub(crate) fn set(&mut self, flag: &str, value: OsString) -> Result<(), String> {
        let value = value
            .into_string()
            .map_err(|_| format!("{flag} must be UTF-8"))?;
        if value.len() > 4096 || value.contains('\0') {
            return Err(format!("{flag} value is invalid or exceeds 4096 bytes"));
        }
        if value.is_empty() || self.values.insert(flag.into(), value.clone()).is_some() {
            return Err(format!("{flag} requires one nonempty value"));
        }
        self.arguments.extend([flag.into(), value.into()]);
        Ok(())
    }

    fn required(&mut self, flag: &str) -> Result<String, String> {
        self.values
            .remove(flag)
            .ok_or_else(|| format!("{flag} is required for the selected mode"))
    }

    fn path(value: String, flag: &str) -> Result<PathBuf, String> {
        let path = PathBuf::from(value);
        if !path.is_absolute() {
            return Err(format!("{flag} requires an absolute path"));
        }
        Ok(path)
    }

    pub(crate) fn finish(self) -> Result<Settings, String> {
        self.finish_for_worker(false)
    }

    pub(crate) fn finish_for_worker(mut self, worker: bool) -> Result<Settings, String> {
        let mode = self
            .values
            .remove("--input")
            .unwrap_or_else(|| "terminal".into());
        let input = match mode.as_str() {
            "terminal" => Input::Terminal,
            "none" => Input::None,
            "pipe" => Input::Pipe,
            "mcp-stdio" => Input::McpStdio,
            "web" | "mcp-http" => {
                let host = self.required("--input-host")?;
                let ip = match host.as_str() {
                    "localhost" => IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
                    _ => host
                        .parse::<IpAddr>()
                        .map_err(|_| "--input-host requires a loopback IP or localhost")?,
                };
                if !ip.is_loopback() {
                    return Err("--input-host must be loopback".into());
                }
                let port = self
                    .required("--input-port")?
                    .parse::<u16>()
                    .ok()
                    .filter(|port| *port > 0)
                    .ok_or("--input-port requires a port in 1..=65535")?;
                let token_file =
                    Self::path(self.required("--input-token-file")?, "--input-token-file")?;
                let settings = Http {
                    address: SocketAddr::new(ip, port),
                    token_file,
                };
                if mode == "web" {
                    Input::Web(settings)
                } else {
                    Input::McpHttp(settings)
                }
            }
            "sock" => {
                if !cfg!(target_os = "linux") {
                    return Err("--input sock requires Linux".into());
                }
                let path = self
                    .values
                    .remove("--input-socket")
                    .map(|value| Self::path(value, "--input-socket"))
                    .transpose()?;
                #[cfg(target_os = "linux")]
                if let Some(path) = &path {
                    crate::local_socket::syntax(path).map_err(|error| error.to_string())?;
                }
                Input::Socket(path)
            }
            _ => {
                return Err(
                    "--input must be terminal, none, pipe, web, mcp-stdio, mcp-http or sock".into(),
                );
            }
        };
        if !cfg!(unix) && input.uses_stdio() {
            return Err(
                "pipe and MCP stdio chat input require Unix; use web or MCP HTTP on Windows".into(),
            );
        }
        let agent = match self
            .values
            .remove("--agent")
            .as_deref()
            .unwrap_or("next-caller")
        {
            "next-caller" => Agent::NextCaller,
            "codex" => Agent::Codex,
            "claude" | "claude-code" => Agent::Claude,
            "opencode" => Agent::OpenCode,
            "exec" => Agent::Exec {
                program: Self::path(self.required("--agent-exec")?, "--agent-exec")?,
                argv_file: self
                    .values
                    .remove("--agent-argv-file")
                    .map(|value| Self::path(value, "--agent-argv-file"))
                    .transpose()?,
            },
            "ssmcp" => {
                let id = self.required("--agent-peer")?;
                crate::wire::messages::parse_recipient(&id)?;
                let key = self.required("--agent-account")?;
                let (provider, subject) = key
                    .split_once(':')
                    .ok_or("--agent-account requires PROVIDER:SUBJECT")?;
                let account = crate::wire::ControlAccount {
                    provider: provider.into(),
                    subject: subject.into(),
                    login: "recipient".into(),
                };
                account.validate()?;
                Agent::Ssmcp {
                    id,
                    account: (account.provider, account.subject),
                }
            }
            _ => {
                return Err(
                    "--agent must be next-caller, codex, claude, opencode, exec or ssmcp".into(),
                );
            }
        };
        if !worker && input == Input::None && agent != Agent::NextCaller {
            return Err("--input none cannot dispatch chat to an explicit agent".into());
        }
        if let Some(flag) = self.values.keys().next() {
            return Err(format!(
                "{flag} does not apply to the selected input or agent"
            ));
        }
        Ok(Settings {
            input,
            agent,
            arguments: self.arguments,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::Options;

    #[test]
    fn input_and_agent_defaults_are_independent() {
        let parse = |flags: &[&str]| {
            Options::parse(
                ["--server", "https://relay.example"]
                    .into_iter()
                    .chain(flags.iter().copied())
                    .map(OsString::from),
            )
            .unwrap()
            .unwrap()
            .chat
        };
        assert_eq!(parse(&[]).input, Input::Terminal);
        assert_eq!(parse(&[]).agent, Agent::NextCaller);
        assert_eq!(parse(&["--agent", "codex"]).input, Input::Terminal);
        let token = std::env::temp_dir().join("chat example token");
        let path = token.to_str().unwrap();
        assert_eq!(
            parse(&[
                "--input",
                "web",
                "--input-host",
                "127.0.0.1",
                "--input-port",
                "8181",
                "--input-token-file",
                path
            ])
            .agent,
            Agent::NextCaller
        );
    }

    #[test]
    fn mode_validation_rejects_ambiguous_or_unsupported_flags() {
        for args in [
            &["--input", "web"][..],
            &[
                "--input",
                "mcp-http",
                "--input-host",
                "0.0.0.0",
                "--input-port",
                "8181",
            ],
            &["--input", "terminal", "--input-port", "8181"],
            &["--agent", "none"],
            &["--watch", "terminal"],
            &["--agent", "ssmcp", "--agent-peer", "alice"],
            &["--agent", "exec", "--agent-exec", "relative"],
        ] {
            assert!(
                Options::parse(
                    ["--server", "https://relay.example"]
                        .into_iter()
                        .chain(args.iter().copied())
                        .map(OsString::from)
                )
                .is_err(),
                "{args:?}"
            );
        }
        for (mode, supported) in [
            ("pipe", cfg!(unix)),
            ("mcp-stdio", cfg!(unix)),
            ("sock", cfg!(target_os = "linux")),
        ] {
            let result = Options::parse(
                ["--server", "https://relay.example", "--input", mode].map(OsString::from),
            );
            assert_eq!(result.is_ok(), supported, "{mode}");
        }
    }

    #[test]
    fn saved_input_and_recipient_keep_literal_flags_and_concrete_origin() {
        let token = std::env::temp_dir().join("input token");
        let arguments = [
            "--input",
            "mcp-http",
            "--input-host",
            "127.0.0.1",
            "--input-port",
            "08181",
            "--input-token-file",
            token.to_str().unwrap(),
            "--agent",
            "claude-code",
        ];
        let options = Options::parse(
            ["--server", "https://relay.example"]
                .into_iter()
                .chain(arguments)
                .map(OsString::from),
        )
        .unwrap()
        .unwrap();
        let spec = options.session_spec().unwrap();
        let saved = Options::parse_saved(&spec).unwrap().unwrap();
        assert_eq!(saved.chat.arguments(), arguments.map(OsString::from));
        assert_eq!(saved.chat, options.chat);
        assert_eq!(saved.server.as_deref(), Some("https://relay.example"));
    }
}

