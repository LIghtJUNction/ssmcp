//! Register the existing authenticated loopback control gateway in Codex.
use super::Settings;
use serde::Deserialize;
use std::{ffi::OsString, net::SocketAddr, process::Command};

const NAME: &str = "ssmcp-local";
const HELP: &str = "Usage: ssmcp mcp install [--harness codex] [--listen IP:PORT]

Register the Linux local control endpoint with Codex. The default listener is
127.0.0.1:8765. Start `ssmcp local --listen IP:PORT` in the workspace to
control. Use the one-time code from /logs when Codex requests authorization.
";

fn listener(args: &[OsString]) -> crate::Result<SocketAddr> {
    let mut address = None;
    let mut harness_seen = false;
    let mut args = args.iter();
    while let Some(flag) = args.next() {
        match flag.to_str() {
            Some("--harness") if !harness_seen => {
                harness_seen = true;
                if args.next().ok_or("--harness requires codex")? != "codex" {
                    return Err("supported local MCP harness: codex".into());
                }
            }
            Some("--listen") if address.is_none() => {
                let raw = args
                    .next()
                    .and_then(|v| v.to_str())
                    .ok_or("--listen requires IP:PORT")?;
                let mut settings = Settings::default();
                settings.set_listen(raw)?;
                if settings.listen.port() == 0 {
                    return Err("installation requires a fixed nonzero loopback port".into());
                }
                address = Some(settings.listen);
            }
            _ => return Err(HELP.into()),
        }
    }
    Ok(address.unwrap_or_else(|| SocketAddr::from(([127, 0, 0, 1], 8765))))
}

#[derive(Deserialize)]
struct Server {
    name: String,
    enabled: bool,
    transport: serde_json::Value,
}

pub(in crate::client) fn run(args: &[OsString]) -> crate::Result<()> {
    if matches!(args, [arg] if arg == "--help" || arg == "-h")
        || matches!(args, [command, arg] if command == "install" && (arg == "--help" || arg == "-h"))
    {
        print!("{HELP}");
        return Ok(());
    }
    let Some((command, options)) = args.split_first() else {
        return Err(HELP.into());
    };
    if command != "install" {
        return Err(HELP.into());
    }
    let listen = listener(options)?;
    let user = crate::broker::username(super::effective_uid())?;
    let url = format!("http://{listen}/{user}");
    let list = Command::new("codex")
        .args(["mcp", "list", "--json"])
        .output()
        .map_err(|error| format!("cannot run codex mcp list: {error}"))?;
    if !list.status.success() {
        return Err(format!(
            "codex mcp list failed ({}); check the Codex configuration",
            list.status
        )
        .into());
    }
    let servers: Vec<Server> = serde_json::from_slice(&list.stdout)?;
    if let Some(server) = servers.iter().find(|server| server.name == NAME) {
        if server.transport["type"] != "streamable_http" || server.transport["url"] != url {
            return Err(format!("Codex entry {NAME} already points to a different server. Inspect it with `codex mcp get {NAME}`.").into());
        }
        if !server.enabled {
            return Err(format!(
                "Codex entry {NAME} is disabled; enable it in your Codex configuration"
            )
            .into());
        }
        println!("Codex already has {NAME} at {url}.");
    } else {
        let status = Command::new("codex")
            .args(["mcp", "add", NAME, "--url", &url])
            .status()?;
        if !status.success() {
            return Err(format!(
                "codex mcp add exited with {status}; inspect `codex mcp get {NAME}`"
            )
            .into());
        }
    }
    println!(
        "Run `ssmcp local --listen {listen}` in the workspace to control. Authorize with `codex mcp login {NAME}` and the one-time code from /logs."
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_accepts_only_fixed_loopback_and_codex() {
        for (raw, accepted) in [
            ("", true),
            ("--harness codex --listen [::1]:9876", true),
            ("--listen 127.0.0.1:0", false),
            ("--listen 0.0.0.0:9876", false),
            ("--listen host.example:9876", false),
            ("--harness other", false),
            ("--listen", false),
            ("--harness codex --harness codex", false),
            ("--listen 127.0.0.1:10 --listen 127.0.0.1:11", false),
        ] {
            let arguments = raw
                .split_whitespace()
                .map(OsString::from)
                .collect::<Vec<_>>();
            assert_eq!(listener(&arguments).is_ok(), accepted, "{raw}");
        }
    }
}
