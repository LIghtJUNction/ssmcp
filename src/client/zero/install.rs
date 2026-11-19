//! Independent user service for the small recovery connection.
mod upgrade;

use super::read_bounded;
use super::{Config, load_config, state_root};
use crate::Result;
use serde_json::{Value, json};
use std::{
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{process::Command, time::timeout};

const UNIT: &str = "ssmcp-007.service";
const MARKER: &str = "# ssmcp-007 managed user service\n";
const OUTPUT_LIMIT: u64 = 32 * 1024;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

const HELP: &str = "Usage: ssmcp 007 install [--server URL] [--name NAME] [--cwd DIR]\n                   [--client ABSOLUTE_PATH] [--dry-run] [--json]\n       ssmcp 007 upgrade [--dry-run] [--json]\n       ssmcp 007 start|stop|status|uninstall [--json]\nInstall preserves the recovery image. Upgrade explicitly replaces it with the\ncurrent CLI, retains a backup and verifies a fresh connected 007 process.\nUninstall retains recovery files and saved sessions.\nFor startup before login and survival after logout, enable user lingering.";

#[derive(Default)]
struct Options {
    server: Option<String>,
    name: Option<String>,
    cwd: Option<PathBuf>,
    client: Option<PathBuf>,
    dry_run: bool,
}

fn parse(args: Vec<OsString>) -> Result<(String, Options)> {
    let mut args = args.into_iter();
    let operation = args
        .next()
        .unwrap_or_else(|| "status".into())
        .into_string()
        .map_err(|_| "007 command must be UTF-8")?;
    let mut options = Options::default();
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--json") => {}
            Some("--dry-run") if matches!(operation.as_str(), "install" | "upgrade") => {
                options.dry_run = true
            }
            Some("--server" | "--name" | "--cwd" | "--client") if operation == "install" => {
                let value = args.next().ok_or("007 option requires a value")?;
                match arg.to_str().unwrap() {
                    "--server" => {
                        options.server =
                            Some(value.into_string().map_err(|_| "server must be UTF-8")?)
                    }
                    "--name" => {
                        options.name = Some(value.into_string().map_err(|_| "name must be UTF-8")?)
                    }
                    "--cwd" => options.cwd = Some(value.into()),
                    "--client" => options.client = Some(value.into()),
                    _ => unreachable!(),
                }
            }
            _ => return Err("unsupported 007 service option; run ssmcp 007 --help".into()),
        }
    }
    if !matches!(
        operation.as_str(),
        "install" | "upgrade" | "start" | "stop" | "status" | "uninstall"
    ) {
        return Err("expected 007 install, upgrade, start, stop, status or uninstall".into());
    }
    Ok((operation, options))
}

fn text(path: &Path) -> Result<&str> {
    if !path.is_absolute() {
        return Err("007 service paths must be absolute".into());
    }
    let value = path.to_str().ok_or("007 service paths must be UTF-8")?;
    if value.chars().any(char::is_control) {
        return Err("007 service paths must not contain control characters".into());
    }
    Ok(value)
}

fn systemd_quote(value: &str, executable: bool) -> Result<String> {
    if value.chars().any(char::is_control) {
        return Err("007 service values must not contain control characters".into());
    }
    let mut escaped = value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%");
    if executable {
        escaped = escaped.replace('$', "$$");
    }
    Ok(format!("\"{escaped}\""))
}

fn definition(
    root: &Path,
    home: &Path,
    path: &str,
    extra_environment: &[(String, String)],
) -> Result<String> {
    let executable = root.join("ssmcp");
    let mut environment = vec![
        ("HOME".to_owned(), text(home)?.to_owned()),
        ("SSMCP_007_HOME".to_owned(), text(root)?.to_owned()),
        ("PATH".to_owned(), path.to_owned()),
    ];
    environment.extend_from_slice(extra_environment);
    let environment = environment
        .into_iter()
        .map(|(key, value)| {
            Ok(format!(
                "Environment={}\n",
                systemd_quote(&format!("{key}={value}"), false)?
            ))
        })
        .collect::<Result<String>>()?;
    Ok(format!(
        "{MARKER}[Unit]\nDescription=ssmcp 007 recovery connection\nStartLimitIntervalSec=0\n\n[Service]\nType=exec\nExecStart={} 007 run\n{environment}Restart=always\nRestartSec=15s\nKillMode=control-group\nTimeoutStopSec=10s\nUMask=0077\nCPUWeight=1\nIOWeight=1\nStandardInput=null\nStandardOutput=journal\nStandardError=journal\nLogRateLimitIntervalSec=5min\nLogRateLimitBurst=10\n\n[Install]\nWantedBy=default.target\n",
        systemd_quote(text(&executable)?, true)?,
    ))
}

fn home() -> Result<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME").ok_or("HOME is unavailable")?);
    text(&home)?;
    Ok(home)
}

fn unit_path(home: &Path) -> Result<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"));
    text(&base)?;
    Ok(base.join("systemd/user").join(UNIT))
}

fn read_unit(path: &Path, root: &Path) -> Result<Option<String>> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.len() > OUTPUT_LIMIT
    {
        return Err(
            "007 service definition must be a bounded regular file owned by this user".into(),
        );
    }
    let mut content = String::new();
    file.take(OUTPUT_LIMIT + 1).read_to_string(&mut content)?;
    let expected_root = format!(
        "Environment={}\n",
        systemd_quote(&format!("SSMCP_007_HOME={}", text(root)?), false)?
    );
    if content.len() > OUTPUT_LIMIT as usize
        || !content.starts_with(MARKER)
        || !content.contains(&expected_root)
    {
        return Err(
            "refusing to modify a service definition not owned by this 007 installation".into(),
        );
    }
    Ok(Some(content))
}

struct Output {
    status: ExitStatus,
    stdout: String,
}

async fn capture(program: &str, args: &[&str]) -> Result<Output> {
    let mut child = Command::new(program)
        .args(args)
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or("missing service command stdout")?;
    let stderr = child
        .stderr
        .take()
        .ok_or("missing service command stderr")?;
    let work = async { tokio::try_join!(child.wait(), read_bounded(stdout), read_bounded(stderr)) };
    let result = timeout(COMMAND_TIMEOUT, work).await;
    match result {
        Ok(Ok((status, stdout, _stderr))) => Ok(Output {
            status,
            stdout: String::from_utf8(stdout)
                .map_err(|_| "service manager returned non-UTF-8 output")?,
        }),
        Ok(Err(error)) => {
            let _ = child.kill().await;
            Err(error.into())
        }
        Err(_) => {
            let _ = child.kill().await;
            Err(
                "007 service manager call timed out; inspect service status before repeating it"
                    .into(),
            )
        }
    }
}

async fn systemctl(args: &[&str]) -> Result<Output> {
    let mut all = vec!["--user", "--no-pager"];
    all.extend_from_slice(args);
    let output = capture("systemctl", &all).await?;
    if !output.status.success() {
        return Err(format!(
            "systemctl --user failed (exit {:?}); inspect {UNIT}",
            output.status.code()
        )
        .into());
    }
    Ok(output)
}

async fn linger() -> Value {
    let uid = unsafe { libc::geteuid() }.to_string();
    match capture(
        "loginctl",
        &["show-user", &uid, "--property=Linger", "--value"],
    )
    .await
    {
        Ok(result) if result.status.success() => match result.stdout.trim() {
            "yes" => json!(true),
            "no" => json!(false),
            _ => Value::Null,
        },
        _ => Value::Null,
    }
}

async fn state(root: &Path, unit: &Path) -> Result<Value> {
    let output = capture("systemctl", &["--user", "--no-pager", "show", UNIT,
        "--property=LoadState,ActiveState,SubState,UnitFileState,MainPID,MemoryCurrent,CPUUsageNSec,NRestarts"]).await?;
    let mut service = serde_json::Map::new();
    for line in output.stdout.lines() {
        if let Some((key, value)) = line.split_once('=') {
            service.insert(key.to_owned(), json!(value));
        }
    }
    let observation = crate::session::read_json::<Value>(&root.join("state.json")).ok();
    let pid = service
        .get("MainPID")
        .and_then(Value::as_str)
        .and_then(|pid| pid.parse::<u64>().ok());
    let active = service.get("ActiveState").and_then(Value::as_str) == Some("active");
    let observation = observation.filter(|value| {
        active && pid.is_some_and(|pid| pid != 0 && value["pid"].as_u64() == Some(pid))
    });
    let connection_state = if active {
        observation
            .as_ref()
            .and_then(|value| value["state"].as_str())
            .unwrap_or("unknown")
    } else {
        "stopped"
    };
    let configured = root.join("config.json").try_exists()?;
    let mut value = json!({
        "unit": UNIT, "unit_path": unit, "state_dir": root,
        "installed": read_unit(unit, root)?.is_some(),
        "configured": configured, "service": service,
        "manager_available": output.status.success(),
        "connection_state": connection_state, "connection_observation": observation,
        "linger": linger().await,
        "recovery_executable": root.join("ssmcp"),
    });
    if configured {
        let config = load_config(root)?;
        value["server"] = json!(config.server);
        value["name"] = json!(config.name);
        value["cwd"] = json!(config.cwd);
        value["client"] = json!(config.client);
    }
    value["versions"] = json!({
        "command":env!("CARGO_PKG_VERSION"),
        "running":observation.as_ref().and_then(|value| value["version"].as_str()),
        "running_wire":observation.as_ref().and_then(|value| value["wire_version"].as_u64()),
        "saved":program_version(&root.join("ssmcp")).await,
    });
    Ok(value)
}

async fn program_version(executable: &Path) -> Option<String> {
    let output = timeout(
        Duration::from_secs(3),
        capture(executable.to_str()?, &["--version"]),
    )
    .await
    .ok()?
    .ok()?;
    let version = output.stdout.trim().strip_prefix("ssmcp ")?;
    (output.status.success()
        && !version.is_empty()
        && version.len() <= 128
        && version
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b".-+_".contains(&byte)))
    .then(|| version.to_owned())
}

fn snapshot(source: &Path, target: &Path) -> Result<bool> {
    match fs::symlink_metadata(target) {
        Ok(metadata) => {
            if !metadata.is_file()
                || metadata.uid() != unsafe { libc::geteuid() }
                || metadata.permissions().mode() & 0o077 != 0
                || metadata.permissions().mode() & 0o100 == 0
            {
                return Err("existing 007 recovery executable must be a private, user-owned executable file".into());
            }
            return Ok(true);
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let mut random = [0u8; 8];
    crate::platform::random_bytes(&mut random)?;
    let suffix = u64::from_ne_bytes(random);
    let temporary = target.with_extension(format!("install-{suffix:x}"));
    let result = (|| -> Result<()> {
        let mut input = File::open(source)?;
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o700)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&temporary)?;
        io::copy(&mut input, &mut output)?;
        output.sync_all()?;
        fs::rename(&temporary, target)?;
        File::open(target.parent().ok_or("recovery executable has no parent")?)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.map(|()| false)
}

fn write_unit(path: &Path, content: &str) -> Result<()> {
    let parent = path.parent().ok_or("unit file has no parent")?;
    fs::create_dir_all(parent)?;
    let mut random = [0u8; 8];
    crate::platform::random_bytes(&mut random)?;
    let temporary = path.with_extension(format!("install-{:x}", u64::from_ne_bytes(random)));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&temporary)?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

async fn install(root: &Path, unit: &Path, home: &Path, options: Options) -> Result<Value> {
    let existing = if root.join("config.json").try_exists()? {
        Some(load_config(root)?)
    } else {
        None
    };
    let server = match options.server {
        Some(server) => super::super::account::normalize_server(&server)?,
        None => match &existing {
            Some(config) => config.server.clone(),
            None => super::super::account::resolve_server(None)?,
        },
    };
    let config = Config {
        format: 1,
        server,
        name: options.name.unwrap_or_else(|| {
            existing
                .as_ref()
                .map_or_else(|| "007".into(), |c| c.name.clone())
        }),
        cwd: options
            .cwd
            .or_else(|| existing.as_ref().map(|c| c.cwd.clone()))
            .unwrap_or(std::env::current_dir()?)
            .canonicalize()?,
        client: options
            .client
            .or_else(|| existing.as_ref().map(|c| c.client.clone()))
            .unwrap_or(std::env::current_exe()?),
    };
    config.validate()?;
    if !config.cwd.is_dir() {
        return Err("007 workspace must be an existing directory".into());
    }
    let client = fs::metadata(&config.client)?;
    if !client.is_file() || client.permissions().mode() & 0o111 == 0 {
        return Err("007 --client must name an installed executable".into());
    }
    let credential = super::super::account::credentials(Some(&config.server))
        .await?
        .ok_or("007 needs an existing saved account; sign in with ssmcp username@server first")?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    if credential.expires_at <= now {
        return Err(
            "007 saved account authorization expired; sign in with ssmcp username@server first"
                .into(),
        );
    }
    let path = std::env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".into());
    let mut extra_environment = Vec::new();
    for key in ["XDG_CONFIG_HOME", "SSMCP_AUTH_HOME", "SSMCP_CLIENT_CONFIG"] {
        if let Some(value) = std::env::var_os(key) {
            let value = PathBuf::from(value);
            extra_environment.push((key.to_owned(), text(&value)?.to_owned()));
        }
    }
    let definition = definition(root, home, &path, &extra_environment)?;
    let previous_unit = read_unit(unit, root)?;
    let changed = existing
        .as_ref()
        .is_some_and(|old| serde_json::to_value(old).ok() != serde_json::to_value(&config).ok())
        || previous_unit.as_ref().is_some_and(|old| old != &definition);
    if options.dry_run {
        return Ok(json!({
            "dry_run": true, "unit": UNIT, "unit_path": unit, "state_dir": root,
            "config": config, "definition": definition,
            "recovery_executable": root.join("ssmcp"),
            "snapshot_preserved": root.join("ssmcp").try_exists()?,
            "linger": linger().await,
        }));
    }
    systemctl(&["show", "--property=Version"]).await?;
    let created_root = state_root(true)?;
    if created_root != root {
        return Err("007 state directory changed during installation".into());
    }
    let _lock = crate::session::lock(&root.join("install.lock"), true)?;
    if read_unit(unit, root)? != previous_unit
        || (root
            .join("config.json")
            .try_exists()?
            .then(|| load_config(root))
            .transpose()?)
            != existing
    {
        return Err(
            "007 installation changed; inspect its state before repeating installation".into(),
        );
    }
    if changed && previous_unit.is_some() {
        let active = capture("systemctl", &["--user", "is-active", UNIT]).await?;
        if active.status.success() || active.stdout.trim() == "activating" {
            return Err("stop 007 before changing its installation; the running recovery executable will be preserved".into());
        }
    }
    let preserved = snapshot(&std::env::current_exe()?, &root.join("ssmcp"))?;
    crate::session::write_json(&root.join("config.json"), &config)?;
    write_unit(unit, &definition)?;
    systemctl(&["daemon-reload"]).await?;
    systemctl(&["enable", "--now", UNIT]).await?;
    let mut result = state(root, unit).await?;
    result["snapshot_preserved"] = json!(preserved);
    result["boot_startup"] = result["linger"].clone();
    Ok(result)
}

pub(super) async fn manage(args: Vec<OsString>) -> Result<()> {
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        println!("{HELP}");
        return Ok(());
    }
    if unsafe { libc::geteuid() } == 0 {
        return Err(
            "007 is a user service; run it as the account that owns your ssmcp login, without sudo"
                .into(),
        );
    }
    let (operation, options) = parse(args)?;
    let root = state_root(false)?;
    let home = home()?;
    let unit = unit_path(&home)?;
    let value = match operation.as_str() {
        "install" => install(&root, &unit, &home, options).await?,
        "upgrade" => upgrade::run(&root, &unit, options.dry_run).await?,
        "status" => state(&root, &unit).await?,
        "start" | "stop" => {
            let _lock = crate::session::lock(&root.join("install.lock"), true)?;
            if read_unit(&unit, &root)?.is_none() {
                return Err("007 user service is not installed".into());
            }
            systemctl(&[operation.as_str(), UNIT]).await?;
            state(&root, &unit).await?
        }
        "uninstall" => {
            let _lock = if root.is_dir() {
                Some(crate::session::lock(&root.join("install.lock"), true)?)
            } else {
                None
            };
            if read_unit(&unit, &root)?.is_some() {
                systemctl(&["disable", "--now", UNIT]).await?;
                fs::remove_file(&unit)?;
                systemctl(&["daemon-reload"]).await?;
            }
            json!({
                "unit":UNIT, "installed":false, "state_dir":root,
                "recovery_files_retained":true, "sessions_retained":true,
            })
        }
        _ => unreachable!(),
    };
    println!("{}", serde_json::to_string_pretty(&value)?);
    if operation == "upgrade" && value["success"] == false {
        return Err(
            "007 upgrade failed; the JSON result reports backup, restoration and connection state"
                .into(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn systemd_values_preserve_literals_and_reject_control_characters() {
        assert_eq!(
            systemd_quote("/tmp/a %b $c\\d\"e", true).unwrap(),
            "\"/tmp/a %%b $$c\\\\d\\\"e\""
        );
        assert_eq!(
            systemd_quote("PATH=$HOME/%h", false).unwrap(),
            "\"PATH=$HOME/%%h\""
        );
        for bad in ["a\nb", "a\rb", "a\0b", "a\tb"] {
            assert!(systemd_quote(bad, true).is_err());
        }
    }

    #[test]
    fn recovery_unit_has_its_own_process_lifetime_and_stable_image() {
        let unit = definition(
            Path::new("/home/alice/007"),
            Path::new("/home/alice"),
            "/usr/bin:/bin",
            &[],
        )
        .unwrap();
        assert!(unit.contains("ExecStart=\"/home/alice/007/ssmcp\" 007 run\n"));
        assert!(unit.contains("StartLimitIntervalSec=0\n"));
        assert!(unit.contains("Restart=always\nRestartSec=15s\n"));
        assert!(unit.contains("KillMode=control-group\n"));
        assert!(!unit.contains("PartOf="));
        assert!(!unit.contains("BindsTo="));
        assert!(!unit.contains("ssmcp.service"));
        assert!(!unit.contains("service run"));
    }

    #[test]
    fn service_operations_reject_install_only_arguments() {
        assert!(
            parse(
                ["start", "--client", "/bin/sh"]
                    .map(OsString::from)
                    .to_vec()
            )
            .is_err()
        );
        assert!(parse(["stop", "--dry-run"].map(OsString::from).to_vec()).is_err());
        assert!(parse(["install", "--server"].map(OsString::from).to_vec()).is_err());
        assert!(parse(["status", "--json"].map(OsString::from).to_vec()).is_ok());
        assert!(
            parse(
                ["upgrade", "--dry-run", "--json"]
                    .map(OsString::from)
                    .to_vec()
            )
            .is_ok()
        );
        assert!(
            parse(
                ["upgrade", "--client", "/bin/sh"]
                    .map(OsString::from)
                    .to_vec()
            )
            .is_err()
        );
    }
}
