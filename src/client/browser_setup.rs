//! Local browser setup. Discovery never approves an extension connection.
use super::config::{self, BrowserSettings};
use crate::{
    Result,
    backend::{playwright, process::Managed},
};
use serde_json::{Value, json};
use std::{
    ffi::OsString,
    io,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{io::AsyncReadExt, process::Command};

const PLAYWRIGHT_VERSION: &str = "0.0.85";
const HELP: &str = "\
ssmcp setup browser [--check] [--json]
ssmcp setup browser --install [--json]
ssmcp setup browser --playwright-mcp /absolute/path/to/cli.js [--json]
                    [--background | --foreground]

Discover existing Chrome/Chromium/Edge extensions and installed browser tools.
The default saves an available Playwright MCP CLI and enables background control.
--check reports the environment without installing or changing settings.
--install installs @playwright/mcp@0.0.85 in the ssmcp user directory using npm.
--playwright-mcp selects an installed native or JavaScript MCP entry.
--foreground explicitly allows the extension to activate browser windows.

Enable Playwright MCP Bridge and approve its first connection in the browser.
A ChatGPT or Claude extension manifest alone exposes no supported ssmcp API.
SSMCP_PLAYWRIGHT_MCP overrides the saved CLI path.";

#[derive(Default, Debug)]
struct Options {
    check: bool,
    install: bool,
    entry: Option<PathBuf>,
    background: Option<bool>,
    json: bool,
    help: bool,
}

fn parse(args: &[OsString]) -> Result<Options> {
    let mut options = Options::default();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--check") => options.check = true,
            Some("--install") => options.install = true,
            Some("--json") => options.json = true,
            Some("--help" | "-h") => options.help = true,
            Some("--background") => {
                if options.background.replace(true) == Some(false) {
                    return Err("--background and --foreground cannot be combined".into());
                }
            }
            Some("--foreground") => {
                if options.background.replace(false) == Some(true) {
                    return Err("--background and --foreground cannot be combined".into());
                }
            }
            Some("--playwright-mcp") => {
                let path = args
                    .next()
                    .ok_or("--playwright-mcp requires an absolute CLI path")?;
                if path.to_string_lossy().starts_with('-') || options.entry.is_some() {
                    return Err("--playwright-mcp requires one absolute CLI path".into());
                }
                let path = PathBuf::from(path);
                if !path.is_absolute() {
                    return Err("--playwright-mcp requires an absolute CLI path".into());
                }
                options.entry = Some(path);
            }
            _ => return Err(format!("invalid browser setup option\n{HELP}").into()),
        }
    }
    if options.check && (options.install || options.entry.is_some() || options.background.is_some())
    {
        return Err("--check cannot be combined with options that change browser settings".into());
    }
    if options.install && options.entry.is_some() {
        return Err("--install and --playwright-mcp cannot be combined".into());
    }
    Ok(options)
}

pub(super) async fn dispatch(args: &[OsString]) -> Result<bool> {
    if !(args.first().is_some_and(|v| v == "setup") && args.get(1).is_some_and(|v| v == "browser"))
    {
        return Ok(false);
    }
    let options = parse(&args[2..])?;
    if options.help {
        println!("{HELP}");
        return Ok(true);
    }
    let report = configure(&options).await?;
    if options.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_report(&report);
    }
    Ok(true)
}

/// SSH setup configures existing browser tools without installing dependencies.
pub(super) async fn configure_existing() -> Result<()> {
    print_report(&configure(&Options::default()).await?);
    Ok(())
}

fn expanded_observed_path(value: &Value) -> Option<PathBuf> {
    let value = value.as_str()?;
    if let Some(suffix) = value.strip_prefix("~/") {
        let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
        Some(PathBuf::from(home).join(suffix))
    } else {
        let path = PathBuf::from(value);
        path.is_absolute().then_some(path)
    }
}

fn candidate_entries(observed: &Value, configured: &BrowserSettings) -> Vec<PathBuf> {
    if let Some(path) = std::env::var_os("SSMCP_PLAYWRIGHT_MCP") {
        return vec![PathBuf::from(path)];
    }
    let mut paths = Vec::new();
    if let Some(path) = &configured.playwright_mcp {
        paths.push(path.clone());
    }
    if let Ok(path) = crate::program::executable("playwright-mcp") {
        paths.push(path);
    }
    if let Some(entries) = observed["playwright_mcp"]["entries"].as_array() {
        for entry in entries.iter().take(16) {
            if let Some(path) = expanded_observed_path(&entry["resolved"]) {
                paths.push(path);
            }
        }
    }
    let mut unique = Vec::new();
    for path in paths {
        if !unique.contains(&path) {
            unique.push(path);
        }
    }
    unique
}

async fn validate_entry(path: &Path, background: bool) -> Result<PathBuf> {
    if !path.is_absolute() || !path.is_file() {
        return Err("Playwright MCP CLI must be an existing absolute file".into());
    }
    let path = path.canonicalize()?;
    let mut command = playwright::command(&path).await.map_err(io::Error::other)?;
    command
        .arg("--help")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut process = Managed::spawn(&mut command)?;
    let stdout = process
        .child
        .stdout
        .take()
        .ok_or("Playwright help output is unavailable")?;
    let result = tokio::time::timeout(Duration::from_secs(10), async {
        let read = async {
            let mut bytes = Vec::new();
            stdout.take(64 * 1024 + 1).read_to_end(&mut bytes).await?;
            if bytes.len() > 64 * 1024 {
                return Err(io::Error::other("Playwright help output exceeds 64 KiB"));
            }
            Ok(bytes)
        };
        tokio::try_join!(process.wait(), read)
    })
    .await;
    let (status, bytes) = match result {
        Ok(Ok(result)) => result,
        Ok(Err(error)) => {
            process.shutdown().await;
            return Err(error.into());
        }
        Err(_) => {
            process.shutdown().await;
            return Err("Playwright MCP CLI check timed out".into());
        }
    };
    if !status.success() {
        return Err("Playwright MCP CLI --help failed; check Node.js and the selected CLI".into());
    }
    let help = String::from_utf8_lossy(&bytes);
    if !help.split_whitespace().any(|word| word == "--extension") {
        return Err("Selected CLI does not support Playwright MCP extension mode".into());
    }
    if background && !help.split_whitespace().any(|word| word == "--background") {
        return Err(
            "Installed Playwright MCP lacks background mode; run ssmcp setup browser --install"
                .into(),
        );
    }
    Ok(path)
}

fn npm_command() -> Result<Command> {
    let npm = crate::program::executable("npm").map_err(
        |_| "npm was not found; install Node.js with npm, then retry ssmcp setup browser --install",
    )?;
    if npm.extension().is_some_and(|extension| extension == "js") {
        let mut command = Command::new(crate::program::node().map_err(io::Error::other)?);
        command.arg(npm);
        return Ok(command);
    }
    #[cfg(windows)]
    if npm
        .extension()
        .is_some_and(|extension| extension == "cmd" || extension == "bat")
    {
        let cli = npm
            .parent()
            .ok_or("npm has no installation directory")?
            .join("node_modules/npm/bin/npm-cli.js");
        if !cli.is_file() {
            return Err(
                "npm.cmd has no adjacent npm-cli.js; install the official Node.js distribution"
                    .into(),
            );
        }
        let mut command = Command::new(crate::program::node().map_err(io::Error::other)?);
        command.arg(cli);
        return Ok(command);
    }
    Ok(Command::new(npm))
}

async fn install(config_path: &Path) -> Result<PathBuf> {
    let directory = config_path
        .parent()
        .ok_or("browser setup requires a configuration directory")?
        .join("browser")
        .join(format!("playwright-{PLAYWRIGHT_VERSION}"));
    let cli = directory.join("node_modules/@playwright/mcp/cli.js");
    if cli.is_file() && validate_entry(&cli, true).await.is_ok() {
        return Ok(cli.canonicalize()?);
    }
    let mut command = npm_command()?;
    std::fs::create_dir_all(&directory)?;
    command
        .arg("install")
        .arg("--prefix")
        .arg(&directory)
        .args([
            "--no-audit",
            "--no-fund",
            "--ignore-scripts",
            "--omit=dev",
            "--save-exact",
        ])
        .arg(format!("@playwright/mcp@{PLAYWRIGHT_VERSION}"))
        .env_remove("NODE_OPTIONS")
        .env_remove("NODE_PATH")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    let mut process = Managed::spawn(&mut command)?;
    let status = match tokio::time::timeout(Duration::from_secs(300), process.wait()).await {
        Ok(status) => status?,
        Err(_) => {
            process.shutdown().await;
            return Err("Playwright MCP installation timed out; rerun setup to retry the user-local installation".into());
        }
    };
    if !status.success() {
        return Err("Playwright MCP installation failed; configuration was not changed".into());
    }
    validate_entry(&cli, true).await
}

fn extension_summary(observed: &Value) -> Value {
    let manifests = observed["extension_manifests"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let entries: Vec<_> = manifests.iter().map(|manifest| {
        let family = manifest["family_hint"].as_str().unwrap_or("unknown");
        json!({
            "family":family, "browser":manifest["browser"], "profile":manifest["profile"],
            "version":manifest["version"], "id":manifest["id"],
            "state":"manifest_present", "enabled":"not_inspected", "paired":"not_inspected",
            "adapter":if family == "playwright" {"playwright_mcp"} else {"no_public_adapter_configured"}
        })
    }).collect();
    json!(entries)
}

fn isolated_desktop() -> Value {
    let tools: serde_json::Map<String, Value> =
        ["Xvfb", "xdotool", "magick", "xprop", "dbus-run-session"]
            .into_iter()
            .map(|name| {
                let path = crate::program::executable(name).ok();
                (
                    name.to_owned(),
                    json!({"installed":path.is_some(),"path":path}),
                )
            })
            .collect();
    let ready = cfg!(target_os = "linux") && tools.values().all(|v| v["installed"] == true);
    json!({"supported":cfg!(target_os = "linux"),"ready":ready,"programs":tools})
}

async fn configure(options: &Options) -> Result<Value> {
    let path = config::config_path()?;
    let mut settings = if path.try_exists()? {
        config::browser()?
    } else {
        BrowserSettings::default()
    };
    if let Some(background) = options.background {
        settings.background = background;
    }
    let observed = tokio::task::spawn_blocking(crate::browser::discovery).await?;
    let mut diagnostics = Vec::<String>::new();
    let selected = if options.install {
        Some(install(&path).await?)
    } else if let Some(entry) = &options.entry {
        Some(validate_entry(entry, settings.background).await?)
    } else {
        let mut selected = None;
        for entry in candidate_entries(&observed, &settings) {
            match validate_entry(&entry, settings.background).await {
                Ok(path) => {
                    selected = Some(path);
                    break;
                }
                Err(error) => diagnostics.push(error.to_string()),
            }
        }
        selected
    };
    let previous = settings.playwright_mcp.clone();
    if let Some(entry) = selected.as_ref() {
        settings.playwright_mcp = Some(entry.clone());
    }
    if !options.check {
        config::set_browser(&settings)?;
    }
    let environment = std::env::var_os("SSMCP_PLAYWRIGHT_MCP").map(PathBuf::from);
    let effective = environment.clone().or_else(|| {
        if options.check {
            previous
        } else {
            settings.playwright_mcp.clone()
        }
    });
    let overridden = environment
        .as_ref()
        .zip(selected.as_ref())
        .is_some_and(|(env, selected)| env.canonicalize().ok().as_ref() != Some(selected));
    let extensions = extension_summary(&observed);
    let has_extension = extensions
        .as_array()
        .is_some_and(|entries| entries.iter().any(|entry| entry["family"] == "playwright"));
    let mut steps = Vec::new();
    if selected.is_none() {
        steps.push("Run ssmcp setup browser --install to install the background-capable Playwright MCP CLI.");
    }
    if overridden {
        steps.push("SSMCP_PLAYWRIGHT_MCP overrides the saved CLI; update or unset it before starting the client.");
    }
    if !has_extension {
        steps.push(
            "Install the official Playwright MCP Bridge extension using extension_install_url.",
        );
    }
    steps.push("Enable Playwright MCP Bridge 0.5.0 or newer and approve the first connection in the browser.");
    if options.check {
        steps.push("Run ssmcp setup browser to save an available CLI path.");
    } else {
        steps.push(
            "Restart the ssmcp client to refresh its native tool catalog after changing setup.",
        );
    }
    Ok(json!({
        "configuration":{"path":path,"saved":!options.check,"background":settings.background,
            "playwright_mcp":if options.check {effective.clone()} else {settings.playwright_mcp.clone()},
            "effective_entry":effective,"environment_override":environment},
        "playwright":{"state":if selected.is_some() {"cli_verified"} else {"cli_unavailable"},
            "entry":selected,"browser_connection":"not_attempted","approval":"browser_native",
            "background_requires_extension":"0.5.0","installed_package":if options.install {Some(PLAYWRIGHT_VERSION)} else {None}},
        "extensions":extensions,
        "extension_install_url":"https://chromewebstore.google.com/detail/playwright-mcp-bridge/mmlmfjhmonkocbjadbfplnigmagldckm",
        "extension_documentation":"https://github.com/microsoft/playwright/blob/main/packages/extension/README.md",
        "native_browser":{"transport":"cdp","uses_system_pointer":false},
        "isolated_desktop":isolated_desktop(),
        "discovery":{"limited":observed["limited"],"diagnostics":observed["diagnostics"]},
        "diagnostics":diagnostics,"next_steps":steps,
        "first_connection":"Chrome may show a connection or approval tab; setup never reads or copies extension tokens."
    }))
}

fn print_report(report: &Value) {
    let configuration = &report["configuration"];
    eprintln!(
        "Browser control: {}",
        report["playwright"]["state"]
            .as_str()
            .unwrap_or("unavailable")
    );
    eprintln!("Background mode: {}", configuration["background"]);
    if let Some(entry) = report["playwright"]["entry"].as_str() {
        eprintln!("Playwright MCP: {entry}");
    }
    eprintln!(
        "Configuration: {}{}",
        configuration["path"].as_str().unwrap_or(""),
        if configuration["saved"] == true {
            " (saved)"
        } else {
            " (read only)"
        }
    );
    for key in ["next_steps", "diagnostics"] {
        if let Some(entries) = report[key].as_array() {
            for entry in entries.iter().filter_map(Value::as_str) {
                eprintln!("- {entry}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn read_only_browser_setup_rejects_mutations() {
        assert!(parse(&args(&["--check", "--json"])).unwrap().check);
        for values in [
            vec!["--check", "--install"],
            vec!["--check", "--foreground"],
            vec!["--background", "--foreground"],
            vec!["--playwright-mcp", "relative.js"],
            vec!["--playwright-mcp", "--install"],
            vec!["--unknown"],
        ] {
            assert!(parse(&args(&values)).is_err(), "{values:?}");
        }
        let options = parse(&args(&["--install", "--background", "--json"])).unwrap();
        assert!(options.install && options.json && options.background == Some(true));
    }

    #[tokio::test]
    async fn unrelated_setup_and_account_commands_are_not_intercepted() {
        for values in [
            vec!["setup"],
            vec!["setup", "host"],
            vec!["login"],
            vec!["browser"],
        ] {
            assert!(!dispatch(&args(&values)).await.unwrap());
        }
    }

    #[test]
    fn extension_metadata_does_not_claim_an_authenticated_control_channel() {
        let observed = json!({"extension_manifests":[
            {"family_hint":"chatgpt","version":"1.0","id":"chatgpt"},
            {"family_hint":"playwright","version":"0.5.0","id":"playwright"}
        ]});
        let entries = extension_summary(&observed);
        assert_eq!(entries[0]["adapter"], "no_public_adapter_configured");
        assert_eq!(entries[0]["paired"], "not_inspected");
        assert_eq!(entries[1]["adapter"], "playwright_mcp");
        assert_eq!(entries[1]["enabled"], "not_inspected");
    }

    #[tokio::test]
    async fn invalid_cli_paths_fail_before_running_any_program() {
        assert!(validate_entry(Path::new("cli.js"), true).await.is_err());
        assert!(
            validate_entry(
                &std::env::temp_dir().join("ssmcp-nonexistent-cli-setup-test"),
                true
            )
            .await
            .is_err()
        );
    }
}
