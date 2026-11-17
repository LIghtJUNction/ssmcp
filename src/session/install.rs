//! Install and control the persistent client session manager through the OS.
//! No command implicitly invokes sudo; system scope is an explicit choice.
use crate::Result;
use serde_json::{Value, json};
use std::{
    ffi::OsString,
    io,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::{
    fs::{self, OpenOptions},
    io::Write,
};
use tokio::process::Command;

#[cfg(target_os = "linux")]
const UNIT: &str = "ssmcp.service";
#[cfg(any(target_os = "macos", test))]
const LABEL: &str = "best.lmm.ssmcp";
const MARKER: &str = "ssmcp client session manager v1";
const LIMIT: usize = 64 * 1024;

#[derive(Default, Debug)]
struct Options {
    operation: String,
    system: bool,
    owner: Option<String>,
    state_dir: Option<PathBuf>,
    json: bool,
    dry_run: bool,
    password_stdin: bool,
}

impl Options {
    fn parse(args: Vec<OsString>) -> Result<Self> {
        let mut args = args.into_iter();
        let operation = args
            .next()
            .unwrap_or_else(|| "status".into())
            .into_string()
            .map_err(|_| "service operation must be UTF-8")?;
        if !matches!(
            operation.as_str(),
            "install" | "uninstall" | "start" | "stop" | "restart" | "status"
        ) {
            return Err("expected service install|uninstall|start|stop|restart|status|run".into());
        }
        let mut options = Self {
            operation,
            ..Self::default()
        };
        while let Some(arg) = args.next() {
            match arg.to_str() {
                Some("--system") if !options.system => options.system = true,
                Some("--user") if options.owner.is_none() => {
                    options.owner = Some(
                        args.next()
                            .ok_or("--user needs an account name")?
                            .into_string()
                            .map_err(|_| "account name must be UTF-8")?,
                    );
                }
                Some("--state-dir") if options.state_dir.is_none() => {
                    options.state_dir = Some(PathBuf::from(
                        args.next().ok_or("--state-dir needs a path")?,
                    ));
                }
                Some("--json") => options.json = true,
                Some("--dry-run") => options.dry_run = true,
                Some("--password-stdin") => options.password_stdin = true,
                _ => {
                    return Err(format!(
                        "unknown or repeated service option: {}",
                        arg.to_string_lossy()
                    )
                    .into());
                }
            }
        }
        if options.dry_run && options.operation != "install" {
            return Err("--dry-run is supported for service install".into());
        }
        if options.state_dir.is_some() && options.operation != "install" {
            return Err("--state-dir is an installation option".into());
        }
        if options.owner.is_some() && (!options.system || options.operation != "install") {
            return Err("--user ACCOUNT selects the owner of a --system installation".into());
        }
        if options.password_stdin
            && !(cfg!(windows) && options.system && options.operation == "install")
        {
            return Err("--password-stdin is only for Windows service install --system".into());
        }
        if let Some(name) = &options.owner {
            validate_text(name)?;
            if name.is_empty() {
                return Err("service account must not be empty".into());
            }
        }
        if let Some(path) = &options.state_dir {
            validate_absolute(path)?;
        }
        Ok(options)
    }
}

pub async fn manage(args: Vec<OsString>) -> Result<()> {
    if args.first().is_some_and(|arg| arg == "run") {
        if args.len() == 1 {
            return super::service::run().await;
        }
        #[cfg(windows)]
        if args.len() == 2 && args[1] == "--scm" {
            return super::windows_service::dispatch().await;
        }
        return Err("expected service run (Windows SCM also accepts --scm)".into());
    }
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        println!(
            "ssmcp service install [--system --user ACCOUNT] [--state-dir PATH] [--dry-run] [--json]\n\
ssmcp service start|stop|restart|status|uninstall [--system] [--json]\n\
ssmcp service run\n\
Default scope: current-user systemd / launchd agent / Windows logon task.\n\
System scope: systemd service / LaunchDaemon / native Windows SCM service.\n\
Windows named-account installation accepts --password-stdin; credentials are never arguments or logs."
        );
        return Ok(());
    }
    let options = Options::parse(args)?;
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let result = manage_unix(&options).await?;
    #[cfg(windows)]
    let result = manage_windows(&options).await?;
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    let result: Value =
        return Err("native client services are supported on Linux, macOS and Windows".into());
    if options.json || options.dry_run {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        println!(
            "ssmcp service · {} · {}",
            result["scope"].as_str().unwrap_or("unknown"),
            result["state"].as_str().unwrap_or("unknown")
        );
        if let Some(definition) = result["definition_path"].as_str() {
            println!("  definition: {definition}");
        }
        if let Some(owner) = result["owner"].as_str() {
            println!("  owner: {owner}");
        }
        if let Some(message) = result["message"].as_str() {
            println!("  {message}");
        }
        if let Some(detail) = result["native"].as_str().filter(|s| !s.trim().is_empty()) {
            println!("{}", detail.trim());
        }
    }
    Ok(())
}

fn validate_text(text: &str) -> Result<()> {
    if text.chars().any(|c| c == '\0' || c == '\r' || c == '\n') {
        return Err("service configuration must not contain NUL or line breaks".into());
    }
    Ok(())
}

fn validate_absolute(path: &Path) -> Result<&str> {
    if !path.is_absolute() {
        return Err("service paths must be absolute".into());
    }
    let text = path.to_str().ok_or("service path must be UTF-8")?;
    validate_text(text)?;
    Ok(text)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(Debug)]
struct Owner {
    name: String,
    home: PathBuf,
    uid: u32,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn owner(account: Option<&str>) -> Result<Owner> {
    use std::ffi::{CStr, CString};
    let account = account.map(CString::new).transpose()?;
    let mut size = 4096;
    loop {
        let mut buffer = vec![0u8; size];
        let mut entry = std::mem::MaybeUninit::<libc::passwd>::zeroed();
        let mut found = std::ptr::null_mut();
        // SAFETY: storage and buffer remain alive through copying both returned strings.
        let code = unsafe {
            if let Some(account) = &account {
                libc::getpwnam_r(
                    account.as_ptr(),
                    entry.as_mut_ptr(),
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                    &mut found,
                )
            } else {
                libc::getpwuid_r(
                    libc::geteuid(),
                    entry.as_mut_ptr(),
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                    &mut found,
                )
            }
        };
        if code == libc::ERANGE && size < 1024 * 1024 {
            size *= 2;
            continue;
        }
        if code != 0 {
            return Err(io::Error::from_raw_os_error(code).into());
        }
        if found.is_null() {
            return Err("service account does not exist".into());
        }
        // SAFETY: getpw*_r returned a populated entry and NUL-terminated strings.
        let entry = unsafe { entry.assume_init() };
        let name = unsafe { CStr::from_ptr(entry.pw_name) }
            .to_str()?
            .to_owned();
        let home = PathBuf::from(unsafe { CStr::from_ptr(entry.pw_dir) }.to_str()?);
        if entry.pw_uid == 0 {
            return Err(
                "client service workers must use a non-root account; use --system --user ACCOUNT"
                    .into(),
            );
        }
        if !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_-.@".contains(c))
        {
            return Err("service account name contains unsupported characters".into());
        }
        validate_absolute(&home)?;
        return Ok(Owner {
            name,
            home,
            uid: entry.pw_uid,
        });
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn installation_owner(options: &Options) -> Result<Owner> {
    if !options.system {
        return owner(None);
    }
    let requested = options
        .owner
        .clone()
        .or_else(|| std::env::var("SUDO_USER").ok().filter(|s| s != "root"));
    owner(Some(requested.as_deref().ok_or(
        "system installation needs --user ACCOUNT or a non-root SUDO_USER",
    )?))
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn owner_path(home: &str) -> String {
    let user =
        format!("{home}/.local/share/mise/shims:{home}/.cargo/bin:{home}/.local/bin:{home}/bin");
    #[cfg(target_os = "macos")]
    {
        format!(
            "{user}:/opt/homebrew/bin:/opt/homebrew/sbin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"
        )
    }
    #[cfg(not(target_os = "macos"))]
    {
        format!("{user}:/usr/local/bin:/usr/bin:/bin:/usr/local/sbin:/usr/sbin:/sbin")
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn state_directory(options: &Options, owner: &Owner) -> Result<PathBuf> {
    let path = if let Some(path) = &options.state_dir {
        path.clone()
    } else if !options.system {
        if let Some(path) = std::env::var_os("SSMCP_SESSION_HOME") {
            PathBuf::from(path)
        } else {
            #[cfg(target_os = "linux")]
            {
                std::env::var_os("XDG_STATE_HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| owner.home.join(".local/state"))
                    .join("ssmcp/sessions")
            }
            #[cfg(target_os = "macos")]
            {
                owner
                    .home
                    .join("Library/Application Support/ssmcp/sessions")
            }
        }
    } else {
        #[cfg(target_os = "linux")]
        {
            owner.home.join(".local/state/ssmcp/sessions")
        }
        #[cfg(target_os = "macos")]
        {
            owner
                .home
                .join("Library/Application Support/ssmcp/sessions")
        }
    };
    validate_absolute(&path)?;
    Ok(path)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn service_path(options: &Options) -> Result<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        if options.system {
            return Ok(PathBuf::from("/etc/systemd/system").join(UNIT));
        }
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or(owner(None)?.home.join(".config"));
        validate_absolute(&base)?;
        Ok(base.join("systemd/user").join(UNIT))
    }
    #[cfg(target_os = "macos")]
    {
        if options.system {
            Ok(PathBuf::from("/Library/LaunchDaemons").join(format!("{LABEL}.plist")))
        } else {
            Ok(owner(None)?
                .home
                .join("Library/LaunchAgents")
                .join(format!("{LABEL}.plist")))
        }
    }
}

#[cfg(any(target_os = "linux", test))]
fn systemd_quote(text: &str, executable: bool) -> Result<String> {
    validate_text(text)?;
    let mut text = text
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%");
    if executable {
        text = text.replace('$', "$$");
    }
    Ok(format!("\"{text}\""))
}

#[cfg(any(target_os = "linux", test))]
fn systemd_definition(
    executable: &str,
    account: &str,
    home: &str,
    state: &str,
    path: &str,
    system: bool,
) -> Result<String> {
    for value in [executable, account, home, state, path] {
        validate_text(value)?;
    }
    let owner = if system {
        format!("User={account}\n")
    } else {
        String::new()
    };
    Ok(format!(
        "# {MARKER}\n[Unit]\nDescription=ssmcp client session manager\nAfter=network-online.target\nWants=network-online.target\n\n[Service]\nType=exec\n{owner}WorkingDirectory={}\nExecStart={} service run\nEnvironment={}\nEnvironment={}\nEnvironment={}\nRestart=on-failure\nRestartSec=5s\nKillMode=control-group\nTimeoutStopSec=20s\nUMask=0077\nStandardInput=null\nStandardOutput=journal\nStandardError=journal\n\n[Install]\nWantedBy={}\n",
        systemd_quote(home, false)?,
        systemd_quote(executable, true)?,
        systemd_quote(&format!("HOME={home}"), false)?,
        systemd_quote(&format!("SSMCP_SESSION_HOME={state}"), false)?,
        systemd_quote(&format!("PATH={path}"), false)?,
        if system {
            "multi-user.target"
        } else {
            "default.target"
        }
    ))
}

#[cfg(any(target_os = "macos", test))]
fn xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(any(target_os = "macos", test))]
fn launchd_definition(
    executable: &str,
    account: &str,
    home: &str,
    state: &str,
    path: &str,
    system: bool,
) -> Result<String> {
    for value in [executable, account, home, state, path] {
        validate_text(value)?;
    }
    let owner = if system {
        format!("<key>UserName</key><string>{}</string>", xml(account))
    } else {
        String::new()
    };
    Ok(format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!-- {MARKER} -->\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict>\n<key>Label</key><string>{LABEL}</string>\n<key>ProgramArguments</key><array><string>{}</string><string>service</string><string>run</string></array>\n{owner}\n<key>WorkingDirectory</key><string>{}</string>\n<key>EnvironmentVariables</key><dict><key>HOME</key><string>{}</string><key>SSMCP_SESSION_HOME</key><string>{}</string><key>PATH</key><string>{}</string></dict>\n<key>RunAtLoad</key><true/>\n<key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>\n<key>ThrottleInterval</key><integer>5</integer>\n<key>ExitTimeOut</key><integer>20</integer>\n<key>ProcessType</key><string>Background</string>\n<key>Umask</key><integer>63</integer>\n</dict></plist>\n",
        xml(executable),
        xml(home),
        xml(home),
        xml(state),
        xml(path)
    ))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn write_definition(path: &Path, text: &str, system: bool) -> Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let parent = path.parent().ok_or("service definition has no parent")?;
    fs::create_dir_all(parent)?;
    if fs::symlink_metadata(parent)?.file_type().is_symlink() {
        return Err("service definition directory must not be a symlink".into());
    }
    if path.try_exists()? {
        verify_owned_definition(path)?;
    }
    let temporary = parent.join(format!(".ssmcp-{}.tmp", super::random_suffix()?));
    let written = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(if system { 0o644 } else { 0o600 })
            .custom_flags(libc::O_NOFOLLOW)
            .open(&temporary)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        drop(file);
        fs::set_permissions(
            &temporary,
            fs::Permissions::from_mode(if system { 0o644 } else { 0o600 }),
        )?;
        fs::rename(&temporary, path)?;
        Ok(())
    })();
    if written.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    written
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn verify_owned_definition(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > LIMIT as u64 {
        return Err("service definition is not a bounded regular file".into());
    }
    if !fs::read_to_string(path)?.contains(MARKER) {
        return Err("existing service definition is not owned by ssmcp; it was not changed".into());
    }
    Ok(())
}

async fn native(
    program: &str,
    args: &[String],
    allow_failure: bool,
) -> Result<std::process::Output> {
    let result = tokio::time::timeout(
        Duration::from_secs(30),
        Command::new(program)
            .args(args)
            .env("LC_ALL", "C")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| "native service manager timed out; query service status before retrying")??;
    if result.stdout.len() > LIMIT || result.stderr.len() > LIMIT {
        return Err("native service manager response exceeds 64 KiB".into());
    }
    if !allow_failure && !result.status.success() {
        let message =
            crate::audit::event(json!({"message":String::from_utf8_lossy(&result.stderr)}));
        return Err(format!(
            "{program} failed (exit {:?}): {}",
            result.status.code(),
            message["message"]
                .as_str()
                .unwrap_or("native manager error")
        )
        .into());
    }
    Ok(result)
}

#[cfg(target_os = "linux")]
fn systemctl_args(options: &Options, operation: &str) -> Vec<String> {
    let mut args = vec!["--no-pager".into()];
    if !options.system {
        args.push("--user".into());
    }
    args.push(operation.into());
    args
}

#[cfg(target_os = "linux")]
async fn systemd_registration(options: &Options, required: bool) -> Result<bool> {
    let mut query = systemctl_args(options, "show");
    query.extend([
        UNIT.into(),
        "--property=LoadState,ActiveState,FragmentPath".into(),
    ]);
    let result = native("systemctl", &query, true).await?;
    let output = String::from_utf8_lossy(&result.stdout);
    let field = |name: &str| {
        output
            .lines()
            .filter_map(|line| line.split_once('='))
            .find_map(|(key, value)| (key == name).then_some(value))
    };
    if field("LoadState") == Some("not-found") {
        return if required {
            Err("ssmcp service is not installed".into())
        } else {
            Ok(false)
        };
    }
    let fragment = field("FragmentPath").filter(|value| !value.is_empty()).ok_or(
        "cannot inspect the native service registration; check the selected systemd manager before installing or controlling it"
    )?;
    // A unit in /usr/lib or another search directory can own this name even
    // when our target definition path is absent. Never shadow such a unit.
    verify_owned_definition(Path::new(fragment))?;
    Ok(matches!(
        field("ActiveState"),
        Some("active" | "activating" | "reloading" | "deactivating")
    ))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn manage_unix(options: &Options) -> Result<Value> {
    if options.password_stdin {
        return Err("--password-stdin is a Windows installation option".into());
    }
    if options.system
        && options.operation != "status"
        && !options.dry_run
        && unsafe { libc::geteuid() } != 0
    {
        return Err("system service changes require an elevated invocation; ssmcp never invokes sudo itself".into());
    }
    let definition_path = service_path(options)?;
    let scope = if options.system { "system" } else { "user" };
    if options.operation == "install" {
        let owner = installation_owner(options)?;
        let executable = std::env::current_exe()?.canonicalize()?;
        let executable = validate_absolute(&executable)?;
        let state = state_directory(options, &owner)?;
        let state = validate_absolute(&state)?;
        let home = validate_absolute(&owner.home)?;
        let path = if !options.system {
            std::env::var("PATH").ok()
        } else {
            None
        }
        .unwrap_or_else(|| owner_path(home));
        validate_text(&path)?;
        #[cfg(target_os = "linux")]
        let definition =
            systemd_definition(executable, &owner.name, home, state, &path, options.system)?;
        #[cfg(target_os = "macos")]
        let definition =
            launchd_definition(executable, &owner.name, home, state, &path, options.system)?;
        #[cfg(target_os = "linux")]
        let commands = {
            let reload = systemctl_args(options, "daemon-reload");
            let mut enable = systemctl_args(options, "enable");
            enable.extend(["--now".into(), UNIT.into()]);
            vec![reload, enable]
        };
        #[cfg(target_os = "macos")]
        let commands = {
            let domain = if options.system {
                "system".into()
            } else {
                format!("gui/{}", owner.uid)
            };
            vec![
                vec!["enable".into(), format!("{domain}/{LABEL}")],
                vec![
                    "bootstrap".into(),
                    domain,
                    validate_absolute(&definition_path)?.into(),
                ],
            ]
        };
        let plan = json!({"manager":if cfg!(target_os="linux") {"systemd"} else {"launchd"}, "scope":scope,
            "state":if options.dry_run {"planned"} else {"installed"},
            "owner":owner.name,"owner_uid":owner.uid,"definition_path":definition_path,
            "session_home":state,"definition":definition,"commands":commands,"dry_run":options.dry_run});
        if options.dry_run {
            return Ok(plan);
        }
        #[cfg(target_os = "linux")]
        let was_running = systemd_registration(options, false).await?;
        write_definition(&definition_path, &definition, options.system)?;
        #[cfg(target_os = "linux")]
        {
            for command in commands {
                native("systemctl", &command, false).await?;
            }
            if was_running {
                let mut restart = systemctl_args(options, "restart");
                restart.push(UNIT.into());
                native("systemctl", &restart, false).await?;
            }
        }
        #[cfg(target_os = "macos")]
        {
            let target = if options.system {
                format!("system/{LABEL}")
            } else {
                format!("gui/{}/{LABEL}", owner.uid)
            };
            let loaded = native("/bin/launchctl", &["print".into(), target.clone()], true)
                .await?
                .status
                .success();
            if loaded {
                native("/bin/launchctl", &["bootout".into(), target], false).await?;
            }
            for command in commands {
                native("/bin/launchctl", &command, false).await?;
            }
        }
        return Ok(plan);
    }
    #[cfg(target_os = "linux")]
    {
        if options.operation != "status" {
            systemd_registration(options, true).await?;
        }
        if options.operation == "uninstall" {
            verify_owned_definition(&definition_path)?;
            let mut disable = systemctl_args(options, "disable");
            disable.extend(["--now".into(), UNIT.into()]);
            native("systemctl", &disable, false).await?;
            fs::remove_file(&definition_path)?;
            native(
                "systemctl",
                &systemctl_args(options, "daemon-reload"),
                false,
            )
            .await?;
            return Ok(
                json!({"scope":scope,"state":"uninstalled","definition_path":definition_path}),
            );
        }
        let mut args = systemctl_args(
            options,
            if options.operation == "status" {
                "show"
            } else {
                &options.operation
            },
        );
        args.push(UNIT.into());
        if options.operation == "status" {
            args.extend([
                "--property=LoadState,ActiveState,SubState,UnitFileState,MainPID,Result,User"
                    .into(),
            ]);
        }
        let result = native("systemctl", &args, options.operation == "status").await?;
        let output = if options.operation == "status" {
            String::from_utf8_lossy(&result.stdout).into_owned()
        } else {
            let mut query = systemctl_args(options, "show");
            query.extend([
                UNIT.into(),
                "--property=LoadState,ActiveState,SubState,UnitFileState,MainPID,Result,User"
                    .into(),
            ]);
            let result = native("systemctl", &query, true).await?;
            String::from_utf8_lossy(&result.stdout).into_owned()
        };
        let fields: serde_json::Map<String, Value> = output
            .lines()
            .filter_map(|line| line.split_once('='))
            .map(|(key, value)| (key.to_owned(), Value::String(value.into())))
            .collect();
        let state = if fields.get("LoadState").and_then(Value::as_str) == Some("not-found") {
            "not-installed"
        } else {
            fields
                .get("ActiveState")
                .and_then(Value::as_str)
                .unwrap_or("unavailable")
        };
        Ok(
            json!({"scope":scope,"state":state,"operation":options.operation,
            "definition_path":definition_path,"manager":"systemd","properties":fields,"native":output}),
        )
    }
    #[cfg(target_os = "macos")]
    {
        let domain = if options.system {
            "system".into()
        } else {
            format!("gui/{}", owner(None)?.uid)
        };
        let target = format!("{domain}/{LABEL}");
        let current = native("/bin/launchctl", &["print".into(), target.clone()], true).await?;
        let loaded = current.status.success();
        match options.operation.as_str() {
            "status" => {
                return Ok(
                    json!({"scope":scope,"state":if loaded {"loaded"} else {"not-loaded"},
                "definition_path":definition_path,"manager":"launchd","native":String::from_utf8_lossy(&current.stdout)}),
                );
            }
            "uninstall" => {
                verify_owned_definition(&definition_path)?;
                if loaded {
                    native("/bin/launchctl", &["bootout".into(), target], false).await?;
                }
                fs::remove_file(&definition_path)?;
            }
            "stop" => {
                if loaded {
                    native("/bin/launchctl", &["bootout".into(), target], false).await?;
                }
            }
            "start" | "restart" => {
                verify_owned_definition(&definition_path)?;
                if loaded {
                    let mut args = vec!["kickstart".into()];
                    if options.operation == "restart" {
                        args.push("-k".into());
                    }
                    args.push(target);
                    native("/bin/launchctl", &args, false).await?;
                } else {
                    native("/bin/launchctl", &["enable".into(), target], false).await?;
                    native(
                        "/bin/launchctl",
                        &[
                            "bootstrap".into(),
                            domain,
                            validate_absolute(&definition_path)?.into(),
                        ],
                        false,
                    )
                    .await?;
                }
            }
            _ => unreachable!(),
        }
        Ok(json!({"scope":scope,"state":options.operation,"definition_path":definition_path}))
    }
}

#[cfg(any(windows, test))]
fn ps_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

#[cfg(any(windows, test))]
fn ps_encoded(script: &str) -> String {
    use base64::{Engine, engine::general_purpose::STANDARD};
    STANDARD.encode(
        script
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>(),
    )
}

#[cfg(windows)]
fn powershell_path() -> Result<String> {
    Ok(std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .ok_or("SystemRoot is unavailable")?
        .join("System32/WindowsPowerShell/v1.0/powershell.exe")
        .to_str()
        .ok_or("PowerShell path must be UTF-8")?
        .into())
}

#[cfg(windows)]
async fn powershell(script: &str) -> Result<Value> {
    let script = format!(
        "$ErrorActionPreference='Stop'; [Console]::OutputEncoding=New-Object System.Text.UTF8Encoding($false); {script}"
    );
    let result = native(
        &powershell_path()?,
        &[
            "-NoLogo".into(),
            "-NoProfile".into(),
            "-NonInteractive".into(),
            "-WindowStyle".into(),
            "Hidden".into(),
            "-EncodedCommand".into(),
            ps_encoded(&script),
        ],
        false,
    )
    .await?;
    Ok(serde_json::from_slice(&result.stdout)?)
}

#[cfg(any(windows, test))]
fn task_script(
    operation: &str,
    task_name: &str,
    executable: &str,
    state: &str,
    powershell: &str,
) -> Result<String> {
    for text in [task_name, executable, state, powershell] {
        validate_text(text)?;
    }
    let prelude = format!(
        "$sid=[Security.Principal.WindowsIdentity]::GetCurrent().User.Value; $s=New-Object -ComObject Schedule.Service; $s.Connect(); $f=$s.GetFolder('\\'); $name={}; $t=$null; try {{$t=$f.GetTask($name)}} catch {{$e=$_.Exception; $missing=$false; while($null -ne $e){{if($e.HResult -eq -2147024894){{$missing=$true}}; $e=$e.InnerException}}; if(-not $missing){{throw}}}}; if($null -ne $t -and $t.Definition.RegistrationInfo.Description -ne {}){{throw 'Task is not owned by ssmcp'}}; ",
        ps_quote(task_name),
        ps_quote(MARKER)
    );
    let action = match operation {
        "install" => {
            let worker = format!("[Environment]::SetEnvironmentVariable('SSMCP_SESSION_HOME',{},'Process'); & {} 'service' 'run'; exit $LASTEXITCODE", ps_quote(state), ps_quote(executable));
            let arguments = format!("-NoLogo -NoProfile -NonInteractive -WindowStyle Hidden -EncodedCommand {}", ps_encoded(&worker));
            format!("if($null -ne $t){{$t.Stop(0)}}; $d=$s.NewTask(0); $d.RegistrationInfo.Description={}; $d.Principal.UserId=$sid; $d.Principal.LogonType=3; $d.Principal.RunLevel=0; $d.Settings.Enabled=$true; $d.Settings.Hidden=$true; $d.Settings.AllowDemandStart=$true; $d.Settings.StartWhenAvailable=$true; $d.Settings.DisallowStartIfOnBatteries=$false; $d.Settings.StopIfGoingOnBatteries=$false; $d.Settings.ExecutionTimeLimit='PT0S'; $d.Settings.MultipleInstances=2; $d.Settings.RestartCount=255; $d.Settings.RestartInterval='PT1M'; $trigger=$d.Triggers.Create(9); $trigger.UserId=$sid; $a=$d.Actions.Create(0); $a.Path={}; $a.Arguments={}; $t=$f.RegisterTaskDefinition($name,$d,6,$sid,$null,3,$null); $null=$t.Run($null); ", ps_quote(MARKER), ps_quote(powershell), ps_quote(&arguments))
        },
        "start" => "if($null -eq $t){throw 'Service task is not installed'}; $t.Enabled=$true; $null=$t.Run($null); ".into(),
        "stop" => "if($null -ne $t){$t.Stop(0)}; ".into(),
        "restart" => "if($null -eq $t){throw 'Service task is not installed'}; $t.Stop(0); $t.Enabled=$true; $null=$t.Run($null); ".into(),
        "uninstall" => "if($null -ne $t){$t.Stop(0); $f.DeleteTask($name,0); $t=$null}; ".into(),
        "status" => String::new(),
        _ => return Err("unknown Windows task operation".into()),
    };
    Ok(format!(
        "{prelude}{action}$state=if($null -eq $t){{'not-installed'}}else{{switch([int]$t.State){{0{{'unknown'}} 1{{'disabled'}} 2{{'queued'}} 3{{'ready'}} 4{{'running'}}}}}}; [ordered]@{{manager='task-scheduler';scope='user';state=$state;task=$name;operation={}}} | ConvertTo-Json -Compress",
        ps_quote(operation)
    ))
}

#[cfg(windows)]
async fn manage_windows(options: &Options) -> Result<Value> {
    if options.system {
        if options.state_dir.is_some() {
            return Err("Windows SCM uses the configured service account profile; --state-dir is available for the user task".into());
        }
        if options.operation == "install" {
            let account = options.owner.as_deref().ok_or("Windows system service installation requires --user ACCOUNT (LocalSystem is not allowed)")?;
            let executable = std::env::current_exe()?.canonicalize()?;
            let account_sid = super::windows_service::account_sid(account)?;
            super::windows_service::validate_account(account, &account_sid)?;
            let plan = json!({"manager":"windows-scm","scope":"system","state":"planned","owner":account,
                "service":"ssmcp","executable":executable,"arguments":["service","run","--scm"],
                "startup":"automatic","dry_run":true,"password_source":if options.password_stdin {"stdin (not retained)"} else {"none"},
                "message":"Service runs under the explicit account profile; it does not access the interactive desktop."});
            if options.dry_run {
                return Ok(plan);
            }
            let password = if options.password_stdin {
                use std::io::Read;
                let mut bytes = Vec::new();
                io::stdin().take(8193).read_to_end(&mut bytes)?;
                if bytes.len() > 8192 {
                    return Err("service password input exceeds 8 KiB".into());
                }
                let mut value =
                    String::from_utf8(bytes).map_err(|_| "service password must be UTF-8")?;
                while value.ends_with(['\r', '\n']) {
                    value.pop();
                }
                if value.contains('\0') {
                    return Err("service password contains NUL".into());
                }
                Some(value)
            } else {
                None
            };
            let outcome = super::windows_service::install(
                account,
                &account_sid,
                password.as_deref(),
                &executable,
            );
            if let Some(mut secret) = password {
                // SAFETY: replacing every UTF-8 byte with zero keeps the string valid.
                unsafe {
                    secret.as_bytes_mut().fill(0);
                }
            }
            return outcome;
        }
        return super::windows_service::control(&options.operation);
    }
    if options.password_stdin {
        return Err("password input is only accepted for a system service".into());
    }
    let identity = powershell("[ordered]@{sid=[Security.Principal.WindowsIdentity]::GetCurrent().User.Value;name=[Security.Principal.WindowsIdentity]::GetCurrent().Name}|ConvertTo-Json -Compress").await?;
    let sid = identity["sid"]
        .as_str()
        .ok_or("Windows user SID unavailable")?;
    if sid == "S-1-5-18" {
        return Err("LocalSystem cannot install a user-session task".into());
    }
    let task = format!("ssmcp-client-{sid}");
    let executable = std::env::current_exe()?.canonicalize()?;
    let state = options
        .state_dir
        .clone()
        .or_else(|| std::env::var_os("SSMCP_SESSION_HOME").map(PathBuf::from))
        .unwrap_or(
            PathBuf::from(std::env::var_os("LOCALAPPDATA").ok_or("LOCALAPPDATA is unavailable")?)
                .join("ssmcp/sessions"),
        );
    let script = task_script(
        &options.operation,
        &task,
        validate_absolute(&executable)?,
        validate_absolute(&state)?,
        &powershell_path()?,
    )?;
    if options.dry_run {
        return Ok(
            json!({"manager":"task-scheduler","scope":"user","state":"planned","owner":identity["name"],
            "task":task,"session_home":state,"trigger":"current-user logon","definition":script,"dry_run":true}),
        );
    }
    powershell(&script).await
}

#[cfg(test)]
mod tests {
    use super::*;
    fn parse(args: &[&str]) -> Result<Options> {
        Options::parse(args.iter().map(OsString::from).collect())
    }

    #[test]
    fn options_do_not_guess_privilege_or_treat_unknown_flags_as_commands() {
        assert!(parse(&["install", "--user", "alice"]).is_err());
        assert!(parse(&["status", "--dry-run"]).is_err());
        assert!(parse(&["start", "--system", "--user", "alice"]).is_err());
        assert!(parse(&["install", "--system", "--user"]).is_err());
        assert!(parse(&["install", "--system", "--unsafe"]).is_err());
        let options = parse(&["install", "--system", "--user", "alice", "--dry-run"]).unwrap();
        assert!(options.system && options.dry_run);
        assert_eq!(options.owner.as_deref(), Some("alice"));
    }

    #[test]
    fn system_scope_path_is_derived_from_owner_home() {
        let path = owner_path("/home/alice");
        assert!(path.starts_with("/home/alice/.local/share/mise/shims:/home/alice/.cargo/bin:"));
        assert!(path.contains("/home/alice/.local/bin:"));
        assert!(!path.contains("/root"));
        assert!(path.contains(":/usr/bin:/bin:"));
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn installed_definitions_replace_only_owned_regular_files() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let root = std::env::temp_dir().join(format!(
            "ssmcp-service-definition-{}",
            super::super::random_suffix().unwrap()
        ));
        fs::create_dir(&root).unwrap();
        let definition = root.join("ssmcp.service");
        let ours = format!("# {MARKER}\n[Service]\n");
        write_definition(&definition, &ours, false).unwrap();
        assert_eq!(
            fs::metadata(&definition).unwrap().permissions().mode() & 0o777,
            0o600
        );
        write_definition(&definition, &format!("{ours}# updated\n"), false).unwrap();
        assert!(
            fs::read_to_string(&definition)
                .unwrap()
                .contains("# updated")
        );
        fs::write(&definition, "foreign definition").unwrap();
        assert!(write_definition(&definition, &ours, false).is_err());
        assert_eq!(
            fs::read_to_string(&definition).unwrap(),
            "foreign definition"
        );
        fs::remove_file(&definition).unwrap();
        let target = root.join("target");
        fs::write(&target, &ours).unwrap();
        symlink(&target, &definition).unwrap();
        assert!(write_definition(&definition, &ours, false).is_err());
        assert_eq!(fs::read_to_string(&target).unwrap(), ours);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn systemd_values_preserve_percent_dollar_quotes_and_spaces() {
        let text = systemd_definition(
            "/opt/my app/$bin%/ssmcp",
            "alice",
            "/home/alice",
            "/home/alice/state%1",
            "/opt/bin:$HOME/bin",
            true,
        )
        .unwrap();
        assert!(text.contains("User=alice\n"));
        assert!(text.contains("ExecStart=\"/opt/my app/$$bin%%/ssmcp\" service run"));
        assert!(text.contains("SSMCP_SESSION_HOME=/home/alice/state%%1"));
        assert!(text.contains("PATH=/opt/bin:$HOME/bin"));
        assert!(text.contains("WantedBy=multi-user.target"));
        assert!(!text.contains("sudo"));
        assert!(
            systemd_definition(
                "/bin/ssmcp\nExecStart=/bin/bad",
                "alice",
                "/home/alice",
                "/state",
                "/bin",
                true
            )
            .is_err()
        );
        let user = systemd_definition(
            "/bin/ssmcp",
            "alice",
            "/home/alice",
            "/state",
            "/bin",
            false,
        )
        .unwrap();
        assert!(!user.contains("User="));
        assert!(user.contains("WantedBy=default.target"));
    }

    #[test]
    fn launchd_uses_persistent_owner_and_literal_argument_elements() {
        let text = launchd_definition(
            "/Applications/My & App/ssmcp",
            "alice",
            "/Users/alice",
            "/state",
            "/bin",
            true,
        )
        .unwrap();
        assert!(text.contains("<key>UserName</key><string>alice</string>"));
        assert!(text.contains("/Applications/My &amp; App/ssmcp"));
        assert!(text.contains("<string>service</string><string>run</string>"));
        assert!(
            !launchd_definition(
                "/bin/ssmcp",
                "alice",
                "/Users/alice",
                "/state",
                "/bin",
                false
            )
            .unwrap()
            .contains("<key>UserName</key>")
        );
    }

    #[test]
    fn windows_task_has_user_logon_trigger_and_quoted_worker() {
        let text = task_script(
            "install",
            "ssmcp-client-S-1-5-21-1",
            "C:\\it's here\\ssmcp.exe",
            "C:\\state's",
            "C:\\Windows\\powershell.exe",
        )
        .unwrap();
        assert!(text.contains("$d.Triggers.Create(9)"));
        assert!(text.contains("$trigger.UserId=$sid"));
        assert!(text.contains("$d.Principal.LogonType=3"));
        assert!(text.contains("$d.Settings.MultipleInstances=2"));
        assert!(!text.contains("C:\\it's here"));
        assert!(task_script("install", "name\nbad", "a", "b", "c").is_err());
        assert_eq!(ps_quote("a'b"), "'a''b'");
    }
}
