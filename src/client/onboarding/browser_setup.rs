//! Explicit browser setup. Inspection never connects to a user's tabs.
use crate::backend::{playwright, process::Managed};
use serde_json::{Value, json};
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{io::AsyncReadExt, process::Command};

const HELP: &str = "Usage: ssmcp setup browser [--check | --install] [--json]\n\n\
Checks the browser environment by default. --install installs the verified\n\
Playwright MCP package in your private ssmcp data directory, then checks it.\n\
Requires Node.js 18+ and npm. Existing browser profiles are preserved.\n\
Install/enable Playwright Extension 0.5.0+ in your chosen Chrome/Edge/Chromium\n\
profile and approve its first connection. Page actions then run in background.\n\
The ChatGPT extension is detected for use by ChatGPT's own Browser tools.\n\
--check and --json never approve a connection or open a browser.\n";

#[derive(Default, Debug, PartialEq)]
struct Options {
    install: bool,
    json: bool,
    help: bool,
}

fn parse(args: &[OsString]) -> Result<Options, String> {
    let mut options = Options::default();
    let mut check = false;
    for arg in args {
        match arg.to_str() {
            Some("--install") if !options.install => options.install = true,
            Some("--check") if !check => check = true,
            Some("--json") if !options.json => options.json = true,
            Some("--help" | "-h") => options.help = true,
            _ => {
                return Err(format!(
                    "Unknown or repeated browser setup argument: {}\n{HELP}",
                    arg.to_string_lossy()
                ));
            }
        }
    }
    if check && options.install {
        return Err("Choose --check or --install, not both.".into());
    }
    Ok(options)
}

pub(crate) async fn run(args: Vec<OsString>) -> crate::Result<()> {
    let options = parse(&args)?;
    if options.help {
        print!("{HELP}");
        return Ok(());
    }
    let installation = if options.install {
        Some(install().await?)
    } else {
        None
    };
    let mut report = inspect().await?;
    report["installation"] = installation.unwrap_or(Value::Null);
    if options.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_report(&report);
    }
    Ok(())
}

async fn output(mut command: Command, deadline: Duration) -> Result<String, String> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut process = Managed::spawn(&mut command)
        .map_err(|e| format!("Could not start browser setup helper: {e}"))?;
    let mut stdout = process
        .child
        .stdout
        .take()
        .ok_or("Missing helper stdout")?
        .take(32769);
    let mut stderr = process
        .child
        .stderr
        .take()
        .ok_or("Missing helper stderr")?
        .take(16385);
    let mut out = Vec::new();
    let mut err = Vec::new();
    let result = tokio::time::timeout(deadline, async {
        let (a, b, status) = tokio::join!(stdout.read_to_end(&mut out), stderr.read_to_end(&mut err), process.wait());
        a.map_err(|e| e.to_string())?;
        b.map_err(|e| e.to_string())?;
        let status = status.map_err(|e| e.to_string())?;
        if out.len() > 32768 || err.len() > 16384 {
            return Err("Browser setup helper exceeded its output budget.".into());
        }
        if !status.success() {
            // npm diagnostics may contain a private registry URL or credentials.
            return Err(format!("Browser setup helper failed ({status}); check the Node/npm installation and registry connectivity."));
        }
        Ok(String::from_utf8_lossy(&out).trim().to_owned())
    }).await.map_err(|_| format!("Browser setup helper exceeded its {}-second deadline.", deadline.as_secs())).and_then(|r| r);
    process.shutdown().await;
    result
}

async fn node_path() -> Result<PathBuf, String> {
    tokio::task::spawn_blocking(crate::program::node)
        .await
        .map_err(|e| e.to_string())?
}

fn npm_command(node: &Path) -> Result<Command, String> {
    let npm = crate::program::executable("npm")?;
    // npm's normal Unix entry is a symlink to this JS file. Windows ships a
    // .cmd wrapper beside node_modules; run the JS entry with validated Node.
    let script = if npm.extension().is_some_and(|ext| ext == "js") {
        npm
    } else {
        let parent = npm.parent().ok_or("npm entry has no parent directory")?;
        [
            parent.join("node_modules/npm/bin/npm-cli.js"),
            parent.join("../lib/node_modules/npm/bin/npm-cli.js"),
        ]
        .into_iter()
        .find(|path| path.is_file())
        .ok_or(
            "npm's installed JavaScript entry is unavailable; install Node.js with npm, then retry",
        )?
    };
    let mut command = Command::new(node);
    command
        .arg(script)
        .env_remove("NODE_OPTIONS")
        .env_remove("NODE_PATH");
    Ok(command)
}

async fn runtime() -> (Value, Value) {
    let node_path = match node_path().await {
        Ok(path) => path,
        Err(error) => {
            return (
                json!({"available":false,"reason":error}),
                json!({"available":false,"reason":"Node.js is required"}),
            );
        }
    };
    let mut command = Command::new(&node_path);
    command
        .arg("--version")
        .env_remove("NODE_OPTIONS")
        .env_remove("NODE_PATH");
    let version = output(command, Duration::from_secs(5)).await;
    let node = match version {
        Ok(version) if playwright::version_at_least(&version, (18, 0, 0)) => {
            json!({"available":true,"path":node_path,"version":version})
        }
        Ok(version) => {
            json!({"available":false,"path":node_path,"version":version,"reason":"Playwright MCP requires Node.js 18 or later"})
        }
        Err(error) => json!({"available":false,"path":node_path,"reason":error}),
    };
    let npm = match npm_command(&node_path) {
        Ok(mut command) => {
            command.arg("--version");
            match output(command, Duration::from_secs(5)).await {
                Ok(version) => json!({"available":true,"version":version}),
                Err(error) => json!({"available":false,"reason":error}),
            }
        }
        Err(error) => json!({"available":false,"reason":error}),
    };
    (node, npm)
}

async fn provider() -> Value {
    let entry = match playwright::installed() {
        Ok(path) => path,
        Err(error) => return json!({"state":"missing","reason":error,"background":true}),
    };
    let version = match playwright::probe(&entry).await {
        Ok(version) => version,
        Err(error) => {
            return json!({"state":"unavailable","entry":entry,"reason":error,"background":true});
        }
    };
    let checked = async {
        let output = crate::output::OutputStore::new_connection()?;
        let server = playwright::Playwright::start_with_output(&entry, output)
            .await
            .map_err(|e| e.to_string())?;
        let tools = server.tools().await;
        server.shutdown().await;
        let tools = tools?;
        if !tools.iter().any(|t| {
            t["name"]
                .as_str()
                .is_some_and(|name| name.starts_with("playwright.browser_"))
        }) {
            return Err("Playwright MCP did not enumerate browser tools".to_owned());
        }
        Ok(tools.len())
    };
    match tokio::time::timeout(Duration::from_secs(20), checked).await {
        Ok(Ok(count)) => {
            json!({"state":"configured","entry":entry,"version":version,"tools":count,"protocol_handshake":"passed","browser_connection":"not_attempted","background":true,"automatic_tab_group":false})
        }
        Ok(Err(error)) => {
            json!({"state":"unavailable","entry":entry,"version":version,"reason":error,"background":true})
        }
        Err(_) => {
            json!({"state":"unavailable","entry":entry,"version":version,"reason":"Playwright MCP inspection exceeded 20 seconds","background":true})
        }
    }
}

fn extension_summary(observed: &Value, family: &str) -> Value {
    let manifests: Vec<_> = observed["extension_manifests"].as_array().into_iter().flatten()
        .filter(|value| value["family_hint"] == family)
        .map(|value| json!({"browser":value["browser"],"profile":value["profile"],"version":value["version"],"id":value["id"]})).collect();
    let compatible = family != "playwright"
        || manifests.iter().any(|m| {
            m["version"]
                .as_str()
                .is_some_and(|v| playwright::version_at_least(v, (0, 5, 0)))
        });
    json!({"manifest_present":!manifests.is_empty(),"background_version_present":compatible && !manifests.is_empty(),"manifests":manifests,"enabled":"not_inspected","connection":"not_attempted"})
}

fn desktop_dependencies() -> Value {
    if !cfg!(target_os = "linux") {
        return json!({"available":false,"reason":"Isolated X11 desktop sessions are available on Linux"});
    }
    let helpers: Vec<_> = ["Xvfb", "xauth", "xdotool", "magick", "xprop", "openbox"]
        .into_iter()
        .map(|name| {
            let path = crate::program::executable(name).ok();
            json!({"name":name,"installed":path.is_some(),"path":path,"optional":name=="openbox"})
        })
        .collect();
    let available = helpers
        .iter()
        .all(|h| h["installed"] == true || h["optional"] == true);
    json!({"available":available,"helpers":helpers,"install_commands":{
        "Arch Linux":"sudo pacman -S --needed xorg-server-xvfb xorg-xauth xdotool imagemagick xorg-xprop openbox",
        "Debian/Ubuntu":"sudo apt install xvfb xauth xdotool imagemagick x11-utils openbox",
        "Fedora":"sudo dnf install xorg-x11-server-Xvfb xorg-x11-xauth xdotool ImageMagick xprop openbox"
    },"note":"The desktop adapter uses ImageMagick's magick entry; older distribution packages may require ImageMagick 7."})
}

fn overall_state(node: &Value, provider: &Value, extension: &Value) -> &'static str {
    if node["available"] != true {
        "needs_runtime"
    } else if provider["state"] != "configured" {
        "needs_provider"
    } else if extension["manifest_present"] != true {
        "needs_extension"
    } else if extension["background_version_present"] != true {
        "extension_update_required"
    } else {
        "awaiting_pairing"
    }
}

async fn inspect() -> Result<Value, String> {
    let browser =
        crate::browser::Browser::new(std::env::current_dir().map_err(|e| e.to_string())?)?;
    let (runtime, provider, discovery, status) = tokio::join!(
        runtime(),
        provider(),
        browser.perform("ssmcp.browser.discover", json!({})),
        browser.perform("ssmcp.browser.status", json!({}))
    );
    let observed = discovery?["structuredContent"].clone();
    let status = status?["structuredContent"].clone();
    let playwright_extension = extension_summary(&observed, "playwright");
    let mut chatgpt = extension_summary(&observed, "chatgpt");
    chatgpt["control"] = json!(
        "Use ChatGPT's official Browser tools; ssmcp does not access the extension's private bridge or credentials."
    );
    let state = overall_state(&runtime.0, &provider, &playwright_extension);
    let next_step = match state {
        "needs_runtime" => {
            "Install Node.js 18 or later with npm, then run ssmcp setup browser --install."
        }
        "needs_provider" => {
            "Run ssmcp setup browser --install to install or repair the managed Playwright MCP provider."
        }
        "needs_extension" => {
            "Install and enable Playwright Extension in the Chrome/Edge/Chromium profile you want to reuse."
        }
        "extension_update_required" => {
            "Update Playwright Extension to version 0.5.0 or later in the selected browser profile."
        }
        _ => {
            "Reload the ssmcp client, discover its playwright.* tools, and approve the browser's first connection."
        }
    };
    Ok(json!({
        "schema_version":1,"state":state,"node":runtime.0,"npm":runtime.1,
        "playwright":provider,"managed_entry":playwright::managed_entry().ok(),
        "operator_override":std::env::var_os("SSMCP_PLAYWRIGHT_MCP").is_some(),
        "extensions":{"playwright":playwright_extension,"chatgpt":chatgpt},
        "browser":{"isolated_executable":status["executable"],"running":observed["running_browsers"],"profiles":observed["profiles"],"discovery_limited":observed["limited"]},
        "desktop":desktop_dependencies(),
        "next_steps":[
            next_step,
            "The browser extension keeps its native approval and tab ownership; its enabled and paired state is checked on the first real connection.",
            "The first connection page may take focus. Background page actions do not move the system pointer or activate tabs after pairing."
        ],
        "extension_url":"https://chromewebstore.google.com/detail/playwright-extension/mmlmfjhmonkocbjadbfplnigmagldckm",
        "documentation":"https://github.com/microsoft/playwright/blob/main/packages/extension/README.md"
    }))
}

fn print_report(report: &Value) {
    println!(
        "Browser setup: {}",
        report["state"].as_str().unwrap_or("unknown")
    );
    for (name, value) in [("Node.js", &report["node"]), ("npm", &report["npm"])] {
        if value["available"] == true {
            println!(
                "  {name}: {}",
                value["version"].as_str().unwrap_or("available")
            );
        } else {
            println!(
                "  {name}: {}",
                value["reason"].as_str().unwrap_or("unavailable")
            );
        }
    }
    let provider = &report["playwright"];
    println!(
        "  Playwright MCP: {}{}",
        provider["state"].as_str().unwrap_or("unavailable"),
        provider["version"]
            .as_str()
            .map(|v| format!(" ({v})"))
            .unwrap_or_default()
    );
    if let Some(reason) = provider["reason"].as_str() {
        println!("    {reason}");
    }
    for family in ["playwright", "chatgpt"] {
        let extension = &report["extensions"][family];
        println!(
            "  {family} extension: {}",
            if extension["manifest_present"] == true {
                "manifest detected; connection not checked"
            } else {
                "not found in standard profiles"
            }
        );
    }
    if report["operator_override"] == true {
        println!(
            "  SSMCP_PLAYWRIGHT_MCP overrides the managed provider; update or remove it if it selects an old installation."
        );
    }
    println!(
        "  Linux isolated desktop: {}",
        if report["desktop"]["available"] == true {
            "helpers installed"
        } else {
            "dependencies unavailable; use --json for package commands"
        }
    );
    println!(
        "\nBackground page actions preserve your system pointer and active tab.\nThe browser's first connection approval may open in front.\nChatGPT's extension remains available through ChatGPT's official Browser tools.\n\nNext: {}\nExtension: {}",
        report["next_steps"][0].as_str().unwrap_or(""),
        report["extension_url"].as_str().unwrap_or("")
    );
}

struct TemporaryDirectory(PathBuf);
impl Drop for TemporaryDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn temporary_directory(parent: &Path) -> Result<TemporaryDirectory, String> {
    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let mut nonce = [0_u8; 12];
    getrandom::fill(&mut nonce).map_err(|e| e.to_string())?;
    let path = parent.join(format!(
        ".install-{}",
        nonce.iter().map(|n| format!("{n:02x}")).collect::<String>()
    ));
    let builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    let mut builder = builder;
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(&path).map_err(|e| e.to_string())?;
    Ok(TemporaryDirectory(path))
}

async fn verify_installation(directory: &Path) -> Result<(), String> {
    let package = directory.join("node_modules/@playwright/mcp/package.json");
    let bytes = std::fs::read(package).map_err(|e| e.to_string())?;
    if bytes.len() > 65536 {
        return Err("Installed package metadata exceeds its budget".into());
    }
    let package: Value = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    if package["name"] != "@playwright/mcp" || package["version"] != playwright::MANAGED_VERSION {
        return Err("Installed Playwright package does not match the requested version".into());
    }
    playwright::probe(&directory.join("node_modules/@playwright/mcp/cli.js")).await?;
    Ok(())
}

async fn install() -> Result<Value, String> {
    let destination = playwright::managed_directory()?;
    if verify_installation(&destination).await.is_ok() {
        return Ok(
            json!({"state":"already_installed","directory":destination,"version":playwright::MANAGED_VERSION}),
        );
    }
    if let Ok(metadata) = std::fs::symlink_metadata(&destination)
        && (!metadata.is_dir() || metadata.file_type().is_symlink())
    {
        return Err(format!(
            "Browser install path is not an ordinary directory: {}",
            destination.display()
        ));
    }
    let node = node_path().await?;
    let mut command = Command::new(&node);
    command
        .arg("--version")
        .env_remove("NODE_OPTIONS")
        .env_remove("NODE_PATH");
    if !playwright::version_at_least(&output(command, Duration::from_secs(5)).await?, (18, 0, 0)) {
        return Err(
            "Playwright MCP requires Node.js 18 or later; update Node.js before --install.".into(),
        );
    }
    let parent = destination
        .parent()
        .ok_or("Browser install path has no parent")?;
    let temporary = temporary_directory(parent)?;
    let staged = temporary.0.join("package");
    let builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    let mut builder = builder;
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(&staged).map_err(|e| e.to_string())?;
    std::fs::write(
        staged.join("package.json"),
        b"{\"name\":\"ssmcp-browser-runtime\",\"private\":true}\n",
    )
    .map_err(|e| e.to_string())?;
    let mut command = npm_command(&node)?;
    command
        .current_dir(&staged)
        .arg("install")
        .arg("--prefix")
        .arg(&staged)
        .args([
            "--global=false",
            "--ignore-scripts",
            "--no-audit",
            "--no-fund",
            "--save-exact",
            "--package-lock",
            "--loglevel=error",
        ])
        .arg(format!("@playwright/mcp@{}", playwright::MANAGED_VERSION));
    // This is the explicit installer, so discovery's offline override is not used.
    output(command, Duration::from_secs(180)).await?;
    verify_installation(&staged).await?;
    let previous = temporary.0.join("previous");
    let replaced = destination.exists();
    if replaced {
        std::fs::rename(&destination, &previous).map_err(|e| e.to_string())?;
    }
    if let Err(error) = std::fs::rename(&staged, &destination) {
        if replaced && let Err(restore) = std::fs::rename(&previous, &destination) {
            let recovery = previous.display().to_string();
            std::mem::forget(temporary);
            return Err(format!(
                "Install failed ({error}); restoring the previous provider failed ({restore}). Its files are preserved at {recovery}."
            ));
        }
        return Err(error.to_string());
    }
    Ok(json!({"state":"installed","directory":destination,"version":playwright::MANAGED_VERSION}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_defaults_to_check_and_rejects_ambiguous_install() {
        assert_eq!(parse(&[]).unwrap(), Options::default());
        assert!(
            parse(&["--install".into(), "--json".into()])
                .unwrap()
                .install
        );
        assert!(parse(&["--check".into(), "--install".into()]).is_err());
        assert!(parse(&["--json".into(), "--json".into()]).is_err());
        assert!(parse(&["--force".into()]).is_err());
    }

    #[test]
    fn extension_detection_never_reports_pairing_as_ready() {
        let node = json!({"available":true});
        let provider = json!({"state":"configured"});
        let discovery = json!({"extension_manifests":[{"family_hint":"playwright","version":"0.4.0"},{"family_hint":"chatgpt","version":"1.26.0"}]});
        let extension = extension_summary(&discovery, "playwright");
        assert_eq!(
            overall_state(&node, &provider, &extension),
            "extension_update_required"
        );
        let discovery =
            json!({"extension_manifests":[{"family_hint":"playwright","version":"0.5.0"}]});
        let extension = extension_summary(&discovery, "playwright");
        assert_eq!(
            overall_state(&node, &provider, &extension),
            "awaiting_pairing"
        );
        assert_eq!(extension["connection"], "not_attempted");
        assert_eq!(
            overall_state(
                &node,
                &provider,
                &extension_summary(&json!({}), "playwright")
            ),
            "needs_extension"
        );
    }
}
