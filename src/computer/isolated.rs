//! A private X server keeps desktop input away from the operator's active session.
//! It is allocated only on request and owned by one client connection.
use super::*;
use std::path::{Path, PathBuf};

#[cfg(target_os = "linux")]
mod display;

#[derive(Clone)]
pub(super) struct Environment {
    pub display: String,
    authority: PathBuf,
    runtime: PathBuf,
}
impl Environment {
    pub fn apply(&self, command: &mut std::process::Command) {
        command
            .env("DISPLAY", &self.display)
            .env("XAUTHORITY", &self.authority)
            .env("XDG_RUNTIME_DIR", &self.runtime)
            .env("XDG_SESSION_TYPE", "x11")
            .env("GDK_BACKEND", "x11")
            .env("QT_QPA_PLATFORM", "xcb")
            .env("SDL_VIDEODRIVER", "x11")
            .env("MOZ_ENABLE_WAYLAND", "0")
            // This private session has no user-systemd activation bus.
            .env("ATSPI_DBUS_IMPLEMENTATION", "dbus-daemon")
            // Preserve GVfs APIs without leaving a FUSE mount in owned state.
            .env("GVFS_DISABLE_FUSE", "1")
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("WAYLAND_SOCKET")
            .env_remove("HYPRLAND_INSTANCE_SIGNATURE")
            .env_remove("DBUS_SESSION_BUS_ADDRESS")
            // libatspi must discover the accessibility bus on our private X11
            // root, never an inherited bus address or a different display.
            .env_remove("AT_SPI_BUS_ADDRESS")
            .env_remove("AT_SPI_DISPLAY")
            .env_remove("DESKTOP_STARTUP_ID")
            .env_remove("XDG_ACTIVATION_TOKEN")
            .env_remove("VSCODE_IPC_HOOK_CLI")
            .env_remove("VSCODE_PID")
            .env_remove("KITTY_LISTEN_ON")
            .env_remove("KITTY_WINDOW_ID")
            .env_remove("WEZTERM_UNIX_SOCKET")
            .env_remove("ALACRITTY_SOCKET")
            .env_remove("NVIM")
            .env_remove("NVIM_LISTEN_ADDRESS")
            .env_remove("EMACS_SERVER_FILE")
            .env_remove("TMUX")
            .env_remove("STY");
    }
}

struct Directory(PathBuf);
impl Drop for Directory {
    fn drop(&mut self) {
        if !self.0.as_os_str().is_empty() {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

/// Remove only the filesystem entries created by this X server. A new server
/// may reuse its display number before our asynchronous reaper finishes.
#[derive(Default)]
struct OwnedX11Files {
    #[cfg(target_os = "linux")]
    entries: Vec<OwnedX11File>,
    // Startup cancellation retains the allocation lock until the child is
    // reaped, even if it had not created its socket when cancellation arrived.
    #[cfg(target_os = "linux")]
    reservation: Option<display::Reservation>,
}
#[cfg(target_os = "linux")]
struct OwnedX11File {
    path: PathBuf,
    device: u64,
    inode: u64,
    socket: bool,
    // Pin the original inode until identity comparison and cleanup finish.
    _pin: std::fs::File,
}
impl OwnedX11Files {
    #[cfg(all(target_os = "linux", test))]
    fn remember(&mut self, path: PathBuf, socket: bool) {
        use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
        // O_PATH pins an unlinked inode without connecting to the socket or
        // following symlinks. Metadata comes from that exact open file.
        if let Ok(pin) = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path)
            && let Ok(metadata) = pin.metadata()
            && if socket {
                metadata.file_type().is_socket()
            } else {
                metadata.file_type().is_file()
            }
        {
            self.entries.push(OwnedX11File {
                path,
                device: metadata.dev(),
                inode: metadata.ino(),
                socket,
                _pin: pin,
            });
        }
    }
}
impl Drop for OwnedX11Files {
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::fs::{FileTypeExt, MetadataExt};
            for entry in &self.entries {
                if let Ok(metadata) = std::fs::symlink_metadata(&entry.path)
                    && metadata.dev() == entry.device
                    && metadata.ino() == entry.inode
                    && if entry.socket {
                        metadata.file_type().is_socket()
                    } else {
                        metadata.file_type().is_file()
                    }
                {
                    let _ = std::fs::remove_file(&entry.path);
                }
            }
        }
    }
}

pub(super) struct Desktop {
    computer: Box<Computer>,
    process: Option<Managed>,
    apps: Vec<Managed>,
    width: u32,
    height: u32,
    // Keep the authorization file until the X server and owned apps stop.
    directory: Directory,
    x11_files: OwnedX11Files,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Start {
    #[serde(default = "default_width")]
    width: u32,
    #[serde(default = "default_height")]
    height: u32,
}
fn default_width() -> u32 {
    1280
}
fn default_height() -> u32 {
    800
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Launch {
    command: Vec<String>,
}

fn required_helpers() -> Value {
    let mut helpers = serde_json::Map::new();
    for name in ["Xvfb", "xdotool", "dbus-run-session", "dbus-daemon"] {
        helpers.insert(
            name.into(),
            match crate::program::executable(name) {
                Ok(path) => json!({"installed":true,"path":path}),
                Err(reason) => json!({"installed":false,"reason":reason}),
            },
        );
    }
    Value::Object(helpers)
}

fn surface(mut response: Value, desktop: &str) -> Value {
    if let Some(metadata) = response
        .get_mut("structuredContent")
        .and_then(Value::as_object_mut)
    {
        metadata.insert("desktop".into(), json!(desktop));
        metadata.insert(
            "input_scope".into(),
            json!(if desktop == "isolated" {
                "private_display"
            } else {
                "host_desktop"
            }),
        );
        metadata.insert("moves_host_pointer".into(), json!(desktop == "host"));
        metadata.insert("host_input_requires_explicit_target".into(), json!(true));
        metadata.insert("host_input_requires_local_handoff".into(), json!(false));
        metadata.insert("host_input_gate".into(), json!("shared_local_switch"));
        let text = serde_json::to_string(metadata).unwrap_or_default();
        if let Some(items) = response.get_mut("content").and_then(Value::as_array_mut)
            && let Some(item) = items.iter_mut().find(|item| item["type"] == "text")
        {
            item["text"] = json!(text);
        }
    }
    response
}

pub(super) fn control_status(mut response: Value, status: Value) -> Value {
    if let Some(metadata) = response
        .get_mut("structuredContent")
        .and_then(Value::as_object_mut)
    {
        metadata.insert("computer_use".into(), status);
        let text = serde_json::to_string(metadata).unwrap_or_default();
        if let Some(items) = response.get_mut("content").and_then(Value::as_array_mut)
            && let Some(item) = items.iter_mut().find(|item| item["type"] == "text")
        {
            item["text"] = json!(text);
        }
    }
    response
}

fn inactive_status() -> Value {
    let helpers = required_helpers();
    let supported = cfg!(target_os = "linux");
    let can_start = supported
        && helpers
            .as_object()
            .is_some_and(|items| items.values().all(|helper| helper["installed"] == true));
    json!({
        "platform":std::env::consts::OS,
        "available":false,"running":false,"desktop":"isolated",
        "input_scope":"private_display","moves_host_pointer":false,
        "host_input_requires_explicit_target":true,
        "supported":supported,"can_start":can_start,"helpers":helpers,
        "automatic_start":supported.then_some("computer.app_launch"),
        "reason":if supported {
            "No private desktop is running. computer.app_launch starts one automatically; use computer.desktop_start first for a custom size."
        } else {
            "Private desktops require Linux and Xvfb on this platform. Explicit desktop: host permits host inspection. All mutations require Computer Use On."
        }
    })
}

fn launch_executable(argv: &[String]) -> Result<PathBuf, String> {
    if argv.is_empty()
        || argv.len() > 128
        || argv[0].is_empty()
        || argv
            .iter()
            .any(|arg| arg.len() > 4096 || arg.contains('\0'))
        || argv.iter().map(String::len).sum::<usize>() > 32768
    {
        return Err("app_launch requires 1..=128 argv items, at most 4096 bytes each and 32 KiB total, without NUL".into());
    }
    crate::program::executable(&argv[0])
}

pub(super) async fn dispatch(
    computer: &Computer,
    name: &str,
    mut args: Value,
) -> Result<Value, String> {
    if computer.closed.load(Ordering::Acquire) {
        return Err("computer connection is closed".into());
    }
    match name {
        "ssmcp.computer.desktop_start" => {
            let options: Start = parse(args)?;
            if !(320..=3840).contains(&options.width) || !(240..=2160).contains(&options.height) {
                return Err("private desktop dimensions must be 320..=3840 by 240..=2160".into());
            }
            let mut slot = computer.isolated.lock().await;
            super::host_control::Permit::check_current()?;
            if let Some(desktop) = slot.as_mut() {
                desktop.ensure_running()?;
                if desktop.width != options.width || desktop.height != options.height {
                    return Err("a private desktop is already running with different dimensions; stop it before resizing".into());
                }
                return Ok(result(desktop.status()));
            }
            let permit =
                super::host_control::Permit::current().ok_or("missing computer-use permit")?;
            let desktop = permit.run(Desktop::start(&computer.root, options)).await?;
            let status = desktop.status();
            permit.commit(|| *slot = Some(desktop))?;
            Ok(result(status))
        }
        "ssmcp.computer.desktop_status" => {
            parse::<Empty>(args)?;
            let mut slot = computer.isolated.lock().await;
            let mut status = if let Some(desktop) = slot.as_mut() {
                let mut status = desktop.status();
                if let Err(error) = desktop.ensure_running() {
                    status["running"] = json!(false);
                    status["error"] = json!(error);
                }
                status
            } else {
                inactive_status()
            };
            status["supported"] = json!(cfg!(target_os = "linux"));
            status["helpers"] = required_helpers();
            status["default_input"] = json!(
                "isolated; app_launch can start a private desktop when Computer Use is On; desktop: host selects the host"
            );
            Ok(result(status))
        }
        "ssmcp.computer.desktop_stop" => {
            parse::<Empty>(args)?;
            let mut slot = computer.isolated.lock().await;
            let stopped = if let Some(mut desktop) = slot.take() {
                desktop.shutdown().await;
                true
            } else {
                false
            };
            Ok(result(
                json!({"stopped":stopped,"desktop":"isolated","host_input_requires_explicit_target":true}),
            ))
        }
        "ssmcp.computer.app_launch" => {
            let launch: Launch = parse(args)?;
            let executable = launch_executable(&launch.command)?;
            let mut slot = computer.isolated.lock().await;
            let permit =
                super::host_control::Permit::current().ok_or("missing computer-use permit")?;
            permit.check()?;
            let created = slot.is_none();
            let mut response = if let Some(desktop) = slot.as_mut() {
                desktop.ensure_running()?;
                desktop.launch(launch.command, executable).await?
            } else {
                let mut desktop = permit
                    .run(Desktop::start(
                        &computer.root,
                        Start {
                            width: default_width(),
                            height: default_height(),
                        },
                    ))
                    .await?;
                match desktop.launch(launch.command, executable).await {
                    Ok(response) => {
                        permit.commit(|| *slot = Some(desktop))?;
                        response
                    }
                    Err(error) => {
                        desktop.shutdown().await;
                        return Err(error);
                    }
                }
            };
            response["structuredContent"]["desktop_started"] = json!(created);
            Ok(surface(response, "isolated"))
        }
        _ => {
            let target = args
                .as_object_mut()
                .ok_or("computer arguments must be an object")?
                .remove("desktop");
            let target = match target.as_ref() {
                None => "isolated",
                Some(Value::String(target)) if matches!(target.as_str(), "host" | "isolated") => {
                    target.as_str()
                }
                _ => return Err("desktop must be isolated or host".into()),
            };
            if target == "host" {
                return computer
                    .perform_host(name, args)
                    .await
                    .map(|r| surface(r, "host"));
            }
            let mut slot = computer.isolated.lock().await;
            super::host_control::Permit::check_current()?;
            if let Some(desktop) = slot.as_mut() {
                desktop.ensure_running()?;
                let operation = desktop.computer.perform_local(name, args);
                let response = if let Some(permit) = super::host_control::Permit::current() {
                    permit.run(operation).await?
                } else {
                    operation.await?
                };
                return Ok(surface(response, "isolated"));
            }
            if name == "ssmcp.computer.status" {
                parse::<Empty>(args)?;
                return Ok(surface(result(inactive_status()), "isolated"));
            }
            Err("no private desktop is running; computer.app_launch starts one automatically, or call computer.desktop_start. Background browser control remains enabled; desktop: host permits observation, while host input also requires the local operator's /computer on switch".into())
        }
    }
}

impl Desktop {
    #[cfg(not(target_os = "linux"))]
    async fn start(_root: &Path, _options: Start) -> Result<Self, String> {
        Err("private desktops currently require Linux and Xvfb; explicit desktop: host selects the host".into())
    }

    #[cfg(target_os = "linux")]
    async fn start(root: &Path, options: Start) -> Result<Self, String> {
        use std::{
            io::Write,
            os::unix::fs::{DirBuilderExt, OpenOptionsExt},
        };
        use tokio::io::{AsyncBufReadExt, BufReader};
        for helper in [
            "Xvfb",
            "xdotool",
            "magick",
            "xprop",
            "dbus-run-session",
            "dbus-daemon",
        ] {
            crate::program::executable(helper)?;
        }
        let mut random = [0u8; 24];
        crate::platform::random_bytes(&mut random).map_err(|e| e.to_string())?;
        let reservation = display::Reservation::acquire(u64::from_ne_bytes(
            random[..8]
                .try_into()
                .expect("eight display selection bytes"),
        ))
        .await?;
        let number = reservation.number();
        let suffix: String = random[..8]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let path =
            std::env::temp_dir().join(format!("ssmcp-desktop-{}-{suffix}", std::process::id()));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .map_err(|e| e.to_string())?;
        let directory = Directory(path);
        let authority = directory.0.join("Xauthority");
        let mut auth = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&authority)
            .map_err(|e| e.to_string())?;
        auth.write_all(&authority_record(&number.to_string(), &random[8..]))
            .map_err(|e| e.to_string())?;
        drop(auth);
        let runtime = directory.0.join("runtime");
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&runtime)
            .map_err(|e| e.to_string())?;
        let environment = Environment {
            display: format!(":{number}"),
            authority: authority.clone(),
            runtime,
        };
        let mut child = Computer::new(root.to_owned())?;
        child.session = Session::X11;
        child.environment = Some(environment);
        let mut command = tokio::process::Command::new(crate::program::executable("Xvfb")?);
        command
            // An explicit display prevents -displayfd from searching from :0.
            .arg(format!(":{number}"))
            .args([
                "-displayfd",
                "1",
                // Reject a partial filesystem/abstract bind in an external race.
                "-nopn",
                "-screen",
                "0",
                &format!("{}x{}x24", options.width, options.height),
                "-nolisten",
                "tcp",
                "-noreset",
                "-auth",
            ])
            .arg(&authority)
            .current_dir(root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        super::host_control::Permit::check_current()?;
        let process = Managed::spawn(&mut command).map_err(|e| format!("Xvfb: {e}"))?;
        // Own startup from the first await: cancellation must retain the cookie
        // and any verified socket inode until this process tree has been reaped.
        let mut desktop = Self {
            computer: Box::new(child),
            process: Some(process),
            apps: Vec::new(),
            width: options.width,
            height: options.height,
            directory,
            x11_files: OwnedX11Files {
                entries: Vec::new(),
                reservation: Some(reservation),
            },
        };
        let ready = async {
            let process = desktop.process.as_mut().ok_or("Xvfb process unavailable")?;
            let stdout = process
                .child
                .stdout
                .take()
                .ok_or("Xvfb display pipe unavailable")?;
            let mut output = String::new();
            tokio::time::timeout(
                Duration::from_secs(5),
                BufReader::new(stdout.take(64)).read_line(&mut output),
            )
            .await
            .map_err(|_| "Xvfb did not announce its display within five seconds".to_owned())?
            .map_err(|error| format!("could not read Xvfb display: {error}"))?;
            if output != format!("{number}\n") {
                return Err("Xvfb did not announce the requested private display".into());
            }
            let files = &mut desktop.x11_files;
            files
                .reservation
                .as_ref()
                .ok_or("private display reservation unavailable")?
                .verify(process, &mut files.entries)
        }
        .await;
        if let Err(error) = ready {
            desktop.shutdown().await;
            return Err(error);
        }
        desktop.x11_files.reservation.take();
        if let Err(error) = desktop.computer.monitors().await {
            desktop.shutdown().await;
            return Err(format!("private display failed readiness check: {error}"));
        }
        Ok(desktop)
    }

    fn status(&self) -> Value {
        json!({"running":true,"desktop":"isolated","display":self.computer.environment.as_ref().map(|env| &env.display),
            "width":self.width,"height":self.height,"input_scope":"private_display",
            "moves_host_pointer":false,"affects_host_focus":false,"owned_apps":self.apps.len(),
            "coordinate_space":"desktop_logical"})
    }

    fn ensure_running(&mut self) -> Result<(), String> {
        match self.process.as_mut().ok_or("private display is stopped")?.child.try_wait() {
            Ok(None) => Ok(()),
            Ok(Some(_)) => Err("private display has exited; stop it and start a new desktop; host input was not used".into()),
            Err(error) => Err(format!("could not inspect private display: {error}")),
        }
    }

    async fn launch(&mut self, argv: Vec<String>, executable: PathBuf) -> Result<Value, String> {
        self.apps
            .retain_mut(|process| matches!(process.child.try_wait(), Ok(None)));
        if self.apps.len() >= 16 {
            return Err("private desktop already owns 16 applications".into());
        }
        let mut command =
            tokio::process::Command::new(crate::program::executable("dbus-run-session")?);
        command.arg("--").arg(&executable).args(&argv[1..]);
        let executable_name = executable
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        if [
            "chromium",
            "chromium-browser",
            "chrome",
            "google-chrome",
            "google-chrome-stable",
            "brave",
            "brave-browser",
            "microsoft-edge",
        ]
        .contains(&executable_name)
        {
            command
                .arg(format!(
                    "--user-data-dir={}",
                    self.directory.0.join("chromium-profile").display()
                ))
                .args([
                    "--no-first-run",
                    "--no-default-browser-check",
                    "--ozone-platform=x11",
                ]);
        } else if matches!(
            executable_name,
            "code" | "code-insiders" | "codium" | "code-oss"
        ) {
            command
                .arg("--new-window")
                .arg("--user-data-dir")
                .arg(self.directory.0.join("editor-profile"));
        } else if matches!(executable_name, "firefox" | "firefox-bin") {
            let profile = self.directory.0.join("firefox-profile");
            std::fs::create_dir_all(&profile).map_err(|e| e.to_string())?;
            command.arg("--no-remote").arg("--profile").arg(profile);
        }
        self.computer
            .environment
            .as_ref()
            .ok_or("private display has no environment")?
            .apply(command.as_std_mut());
        command
            .env("XDG_CONFIG_HOME", self.directory.0.join("config"))
            .env("XDG_CACHE_HOME", self.directory.0.join("cache"))
            .env("XDG_DATA_HOME", self.directory.0.join("data"))
            .current_dir(&self.computer.root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        super::host_control::Permit::check_current()?;
        let process = Managed::spawn(&mut command)
            .map_err(|e| format!("could not launch private application: {e}"))?;
        let mut process = process;
        if let Err(error) = super::host_control::Permit::check_current() {
            process.shutdown().await;
            return Err(error);
        }
        let supervisor_pid = process.child.id();
        self.apps.push(process);
        Ok(result(
            json!({"started":true,"supervisor_pid":supervisor_pid,"desktop":"isolated",
            "display":self.computer.environment.as_ref().map(|env| &env.display),"input_scope":"private_display",
            "moves_host_pointer":false,"window_readiness":"inspect computer.windows before input"}),
        ))
    }

    pub(super) async fn shutdown(&mut self) {
        let _ = self.computer.shutdown_local().await;
        for app in &mut self.apps {
            app.shutdown().await;
        }
        self.apps.clear();
        if let Some(process) = self.process.as_mut() {
            process.shutdown().await;
        }
        self.process.take();
    }
}

impl Drop for Desktop {
    fn drop(&mut self) {
        let Some(process) = self.process.take() else {
            return;
        };
        let apps = std::mem::take(&mut self.apps);
        let directory = Directory(std::mem::take(&mut self.directory.0));
        let x11_files = std::mem::take(&mut self.x11_files);
        reap_desktop(process, apps, directory, x11_files);
    }
}

fn reap_desktop(
    mut process: Managed,
    mut apps: Vec<Managed>,
    directory: Directory,
    x11_files: OwnedX11Files,
) {
    for app in &mut apps {
        app.terminate();
    }
    process.terminate();
    // Managed's guardian exits only after its descendants have been reaped.
    // Keep authorization, state and socket guards until that point.
    std::thread::spawn(move || {
        let wait = |process: &mut Managed| loop {
            match process.child.try_wait() {
                Ok(Some(_)) => break true,
                Ok(None) => std::thread::sleep(Duration::from_millis(20)),
                Err(_) => break false,
            }
        };
        let mut apps_reaped = true;
        for app in &mut apps {
            // Every child must be observed even when an earlier wait failed.
            apps_reaped &= wait(app);
        }
        let server_reaped = wait(&mut process);
        drop(apps);
        drop(process);
        if apps_reaped && server_reaped {
            drop(x11_files);
            drop(directory);
        } else {
            // A failed wait is not evidence that it is safe to remove live state.
            eprintln!("ssmcp: private desktop reaping failed; retaining owned state");
            std::mem::forget(x11_files);
            std::mem::forget(directory);
        }
    });
}

// Xauthority's binary record uses big-endian lengths and FamilyWild so the
// random cookie works with either local hostname form without opening TCP.
#[cfg(target_os = "linux")]
fn authority_record(display: &str, cookie: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(64);
    bytes.extend_from_slice(&u16::MAX.to_be_bytes());
    for field in [
        &b""[..],
        display.as_bytes(),
        &b"MIT-MAGIC-COOKIE-1"[..],
        cookie,
    ] {
        bytes.extend_from_slice(&(field.len() as u16).to_be_bytes());
        bytes.extend_from_slice(field);
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_command_removes_session_overrides_without_changing_parent() {
        use std::ffi::OsStr;
        let before: Vec<_> = [
            "DISPLAY",
            "WAYLAND_DISPLAY",
            "WAYLAND_SOCKET",
            "XAUTHORITY",
            "AT_SPI_BUS_ADDRESS",
            "AT_SPI_DISPLAY",
            "ATSPI_DBUS_IMPLEMENTATION",
            "GVFS_DISABLE_FUSE",
            "GIO_USE_VFS",
        ]
        .into_iter()
        .map(|name| (name, std::env::var_os(name)))
        .collect();
        let environment = Environment {
            display: ":10000".into(),
            authority: PathBuf::from("private-cookie"),
            runtime: PathBuf::from("private-runtime"),
        };
        let mut command = std::process::Command::new("not-executed");
        command
            .env("WAYLAND_DISPLAY", "fixture-wayland")
            .env("WAYLAND_SOCKET", "91")
            .env(
                "AT_SPI_BUS_ADDRESS",
                "unix:path=/unused-host-accessibility-bus",
            )
            .env("AT_SPI_DISPLAY", ":1")
            .env("ATSPI_DBUS_IMPLEMENTATION", "dbus-broker")
            .env("GVFS_DISABLE_FUSE", "")
            .env("GIO_USE_VFS", "gvfs");
        environment.apply(&mut command);
        let overrides: std::collections::HashMap<_, _> = command.get_envs().collect();
        assert_eq!(
            overrides.get(OsStr::new("DISPLAY")),
            Some(&Some(OsStr::new(":10000")))
        );
        assert_eq!(
            overrides.get(OsStr::new("XDG_SESSION_TYPE")),
            Some(&Some(OsStr::new("x11")))
        );
        assert_eq!(overrides.get(OsStr::new("WAYLAND_DISPLAY")), Some(&None));
        assert_eq!(overrides.get(OsStr::new("WAYLAND_SOCKET")), Some(&None));
        assert_eq!(overrides.get(OsStr::new("AT_SPI_BUS_ADDRESS")), Some(&None));
        assert_eq!(overrides.get(OsStr::new("AT_SPI_DISPLAY")), Some(&None));
        assert_eq!(
            overrides.get(OsStr::new("ATSPI_DBUS_IMPLEMENTATION")),
            Some(&Some(OsStr::new("dbus-daemon")))
        );
        assert_eq!(
            overrides.get(OsStr::new("GVFS_DISABLE_FUSE")),
            Some(&Some(OsStr::new("1")))
        );
        assert_eq!(
            overrides.get(OsStr::new("GIO_USE_VFS")),
            Some(&Some(OsStr::new("gvfs")))
        );
        for (name, expected) in before {
            assert_eq!(std::env::var_os(name), expected, "{name}");
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn owned_x11_files_remove_the_same_inode_and_preserve_replacements() {
        use std::os::unix::net::UnixListener;
        let mut nonce = [0u8; 8];
        crate::platform::random_bytes(&mut nonce).unwrap();
        let suffix: String = nonce.iter().map(|byte| format!("{byte:02x}")).collect();
        let directory = Directory(std::env::temp_dir().join(format!("ssmcp-x11-files-{suffix}")));
        std::fs::create_dir(&directory.0).unwrap();
        let socket = directory.0.join("X1");
        let lock = directory.0.join(".X1-lock");
        let original = UnixListener::bind(&socket).unwrap();
        std::fs::write(&lock, "1234").unwrap();
        let mut owned = OwnedX11Files::default();
        owned.remember(socket.clone(), true);
        owned.remember(lock.clone(), false);
        drop(original);
        drop(owned);
        assert!(!socket.exists());
        assert!(!lock.exists());

        let original = UnixListener::bind(&socket).unwrap();
        let mut owned = OwnedX11Files::default();
        owned.remember(socket.clone(), true);
        std::fs::remove_file(&socket).unwrap();
        drop(original);
        // Only the production guard pins the unlinked original inode now.
        let replacement = UnixListener::bind(&socket).unwrap();
        drop(owned);
        assert!(
            socket.exists(),
            "a newer X server's socket must survive cleanup"
        );
        drop(replacement);
    }

    #[tokio::test]
    async fn input_never_silently_targets_host() {
        let computer = Computer::new(std::env::temp_dir()).unwrap();
        let error = computer
            .perform("ssmcp.computer.move", json!({"x":10,"y":10}))
            .await
            .unwrap_err();
        assert!(error.contains("no private desktop"));
        assert!(
            computer
                .perform("ssmcp.computer.screenshot", json!({"desktop":"isolated"}))
                .await
                .unwrap_err()
                .contains("no private desktop")
        );
        // This assertion checks routing, never real foreground input.
        computer.set_host_input_local(false).unwrap();
        assert!(
            computer
                .perform("ssmcp.computer.key", json!({"desktop":"host","key":""}))
                .await
                .is_err()
        );
        assert!(
            computer
                .perform("ssmcp.computer.status", json!({"desktop":"other"}))
                .await
                .unwrap_err()
                .contains("desktop must")
        );
    }

    #[test]
    fn desktop_metadata_describes_shared_gate_without_timed_handoff() {
        for (desktop, input_scope, moves_host_pointer) in [
            ("host", "host_desktop", true),
            ("isolated", "private_display", false),
        ] {
            let response = surface(crate::backend::result(json!({"available": true})), desktop);
            let metadata = &response["structuredContent"];
            assert_eq!(metadata["desktop"], desktop);
            assert_eq!(metadata["input_scope"], input_scope);
            assert_eq!(metadata["moves_host_pointer"], moves_host_pointer);
            assert_eq!(metadata["host_input_requires_explicit_target"], true);
            assert_eq!(metadata["host_input_requires_local_handoff"], false);
            assert_eq!(metadata["host_input_gate"], "shared_local_switch");
            assert!(metadata.get("background_enabled").is_none());
            let text = response["content"]
                .as_array()
                .unwrap()
                .iter()
                .find(|item| item["type"] == "text")
                .unwrap()["text"]
                .as_str()
                .unwrap();
            assert_eq!(serde_json::from_str::<Value>(text).unwrap(), *metadata);
        }
    }

    #[tokio::test]
    async fn validates_dimensions_before_spawning() {
        let computer = Computer::new(std::env::temp_dir()).unwrap();
        assert!(
            computer
                .perform("ssmcp.computer.desktop_start", json!({"width":100000}))
                .await
                .unwrap_err()
                .contains("dimensions")
        );
        assert_eq!(
            computer
                .perform("ssmcp.computer.desktop_stop", json!({}))
                .await
                .unwrap()["structuredContent"]["stopped"],
            false
        );
    }

    #[tokio::test]
    async fn default_observation_and_input_never_enter_the_host_adapter_or_start_a_display() {
        let computer = Computer::new(std::env::temp_dir()).unwrap();
        // perform_local acquires this mutex before inspecting any host adapter.
        // A fallback regression times out here instead of touching the desktop.
        let host_guard = computer.serial.lock().await;
        for name in ["ssmcp.computer.status", "ssmcp.computer.desktop_status"] {
            let response =
                tokio::time::timeout(Duration::from_secs(1), computer.perform(name, json!({})))
                    .await
                    .expect("default status must not enter the host adapter")
                    .unwrap();
            let state = &response["structuredContent"];
            assert_eq!(state["desktop"], "isolated");
            assert_eq!(state["running"], false);
            assert_eq!(state["moves_host_pointer"], false);
            assert!(state["can_start"].is_boolean());
            assert!(state["helpers"].is_object());
            assert_eq!(state["supported"], cfg!(target_os = "linux"));
            assert!(computer.isolated.lock().await.is_none());
        }
        for (name, arguments) in [
            ("monitors", json!({})),
            ("windows", json!({})),
            ("screenshot", json!({})),
            ("focus", json!({"window_id":"1"})),
            ("move", json!({"x":1,"y":1})),
            ("click", json!({})),
            ("drag", json!({"path":[{"x":0,"y":0},{"x":1,"y":1}]})),
            ("key", json!({"key":"Return"})),
            ("type", json!({"text":"private fixture"})),
            ("scroll", json!({"direction":"down"})),
        ] {
            for explicit in [false, true] {
                let mut arguments = arguments.clone();
                if explicit {
                    arguments["desktop"] = json!("isolated");
                }
                let outcome = tokio::time::timeout(
                    Duration::from_secs(1),
                    computer.perform(&format!("ssmcp.computer.{name}"), arguments),
                )
                .await
                .expect("default operations must not enter the host adapter");
                assert!(
                    outcome.unwrap_err().contains("no private desktop"),
                    "{name}"
                );
                assert!(computer.isolated.lock().await.is_none());
            }
        }
        drop(host_guard);
        computer.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn app_launch_rejects_invalid_argv_and_missing_executable_before_desktop_start() {
        let computer = Computer::new(std::env::temp_dir()).unwrap();
        // Startup needs this lock. Invalid requests must fail before acquiring
        // it, without starting Xvfb or relying on the host's installed helpers.
        let private_guard = computer.isolated.lock().await;
        for arguments in [
            json!({"command":[]}),
            json!({"command":[""]}),
            json!({"command":["invalid\0"]}),
            json!({"command":["unused","x".repeat(4097)]}),
            json!({"command":vec!["unused";129]}),
            json!({"command":vec!["x".repeat(4096);9]}),
            json!({"command":["unused"],"desktop":"host"}),
            json!({"command":"not-an-argv"}),
        ] {
            let error = tokio::time::timeout(
                Duration::from_secs(1),
                computer.perform("ssmcp.computer.app_launch", arguments),
            )
            .await
            .expect("invalid argv must fail before locking or starting a desktop")
            .unwrap_err();
            assert!(
                !error.contains("Xvfb"),
                "argv validation must precede dependency/startup checks"
            );
            assert!(private_guard.is_none());
        }
        let mut nonce = [0u8; 16];
        crate::platform::random_bytes(&mut nonce).unwrap();
        let missing = std::env::temp_dir().join(format!(
            "ssmcp-missing-app-{}",
            nonce
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        ));
        let error = tokio::time::timeout(
            Duration::from_secs(1),
            computer.perform("ssmcp.computer.app_launch", json!({"command":[missing]})),
        )
        .await
        .expect("an unresolved executable must not acquire the desktop lock")
        .unwrap_err();
        assert!(error.contains("not installed") || error.contains("No such file"));
        assert!(private_guard.is_none());
        drop(private_guard);
        computer.shutdown().await.unwrap();
    }

    #[test]
    fn desktop_schema_keeps_observation_read_only_and_launch_explicitly_mutating() {
        let tools = Computer::tools();
        for name in [
            "status",
            "desktop_status",
            "monitors",
            "windows",
            "screenshot",
        ] {
            let tool = tools
                .iter()
                .find(|tool| tool["name"] == format!("ssmcp.computer.{name}"))
                .unwrap();
            assert_eq!(tool["annotations"]["readOnlyHint"], true, "{name}");
            if name != "desktop_status" {
                assert_eq!(
                    tool["inputSchema"]["properties"]["desktop"]["default"],
                    "isolated"
                );
            }
        }
        for name in [
            "desktop_start",
            "desktop_stop",
            "app_launch",
            "move",
            "click",
            "key",
            "type",
        ] {
            let tool = tools
                .iter()
                .find(|tool| tool["name"] == format!("ssmcp.computer.{name}"))
                .unwrap();
            assert_eq!(tool["annotations"]["readOnlyHint"], false, "{name}");
        }
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[ignore = "requires installed Xvfb, xdotool, dbus-run-session and true; private display only"]
    async fn app_launch_auto_starts_private_display_and_releases_owned_state() {
        use std::os::unix::fs::MetadataExt;
        let host_environment: Vec<_> = [
            "DISPLAY",
            "WAYLAND_DISPLAY",
            "XAUTHORITY",
            "DBUS_SESSION_BUS_ADDRESS",
        ]
        .into_iter()
        .map(|key| (key, std::env::var_os(key)))
        .collect();
        let computer = Computer::new(std::env::temp_dir()).unwrap();
        assert!(computer.isolated.lock().await.is_none());
        let executable = crate::program::executable("true").expect("true fixture executable");
        let started = computer
            .perform("ssmcp.computer.app_launch", json!({"command":[executable]}))
            .await
            .unwrap();
        assert_eq!(started["structuredContent"]["desktop_started"], true);
        assert_eq!(started["structuredContent"]["moves_host_pointer"], false);
        let status = computer
            .perform("ssmcp.computer.desktop_status", json!({}))
            .await
            .unwrap();
        assert_eq!(status["structuredContent"]["width"], 1280);
        assert_eq!(status["structuredContent"]["height"], 800);
        let (directory, display, owned_files) = {
            let slot = computer.isolated.lock().await;
            let desktop = slot.as_ref().unwrap();
            (
                desktop.directory.0.clone(),
                desktop
                    .computer
                    .environment
                    .as_ref()
                    .unwrap()
                    .display
                    .clone(),
                desktop
                    .x11_files
                    .entries
                    .iter()
                    .map(|file| (file.path.clone(), file.device, file.inode))
                    .collect::<Vec<_>>(),
            )
        };
        assert_ne!(
            Some(std::ffi::OsString::from(&display)),
            std::env::var_os("DISPLAY")
        );
        let reused = computer
            .perform("ssmcp.computer.app_launch", json!({"command":["true"]}))
            .await
            .unwrap();
        assert_eq!(reused["structuredContent"]["desktop_started"], false);
        assert_eq!(reused["structuredContent"]["display"], display);
        computer.shutdown().await.unwrap();
        assert!(
            !directory.exists(),
            "owned private files must be removed after shutdown"
        );
        for (path, device, inode) in owned_files {
            if let Ok(metadata) = std::fs::symlink_metadata(path) {
                assert_ne!(
                    (metadata.dev(), metadata.ino()),
                    (device, inode),
                    "owned X11 files were retained"
                );
            }
        }
        for (key, expected) in host_environment {
            assert_eq!(std::env::var_os(key), expected, "{key}");
        }
        assert!(computer.isolated.lock().await.is_none());
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[ignore = "requires installed Xvfb, xdotool, wish and dbus-run-session; private display only"]
    async fn private_display_input_capture_and_cleanup() {
        let host_display = std::env::var_os("DISPLAY");
        let computer = Computer::new(std::env::temp_dir()).unwrap();
        let started = computer
            .perform(
                "ssmcp.computer.desktop_start",
                json!({"width":640,"height":480}),
            )
            .await
            .unwrap();
        let display = started["structuredContent"]["display"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_ne!(Some(std::ffi::OsString::from(&display)), host_display);
        computer
            .perform("ssmcp.computer.move", json!({"x":31,"y":42}))
            .await
            .unwrap();
        let directory = {
            let slot = computer.isolated.lock().await;
            let desktop = slot.as_ref().unwrap();
            let pointer = desktop
                .computer
                .text("xdotool", &["getmouselocation", "--shell"])
                .await
                .unwrap();
            assert!(
                pointer.contains("X=31") && pointer.contains("Y=42"),
                "{pointer}"
            );
            desktop.directory.0.clone()
        };
        let script = directory.join("fixture.tcl");
        let observed = directory.join("typed.txt");
        std::fs::write(&script, format!(
            "wm title . {{ssmcp private fixture}}\nwm geometry . 440x180+20+20\nentry .input\npack .input -fill x -padx 20 -pady 20\nfocus -force .input\nbind .input <KeyRelease> {{set f [open {{{}}} w]; puts -nonewline $f [.input get]; close $f}}\n",
            observed.display()
        )).unwrap();
        computer
            .perform(
                "ssmcp.computer.app_launch",
                json!({"command":["wish",script]}),
            )
            .await
            .unwrap();
        let window = tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                let windows = computer
                    .perform("ssmcp.computer.windows", json!({}))
                    .await
                    .unwrap();
                if let Some(window) = windows["structuredContent"]["windows"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|window| window["title"] == "ssmcp private fixture")
                {
                    break window["window_id"].as_str().unwrap().to_owned();
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("private fixture window must appear");
        computer
            .perform("ssmcp.computer.focus", json!({"window_id":window}))
            .await
            .unwrap();
        assert!(
            computer
                .perform("ssmcp.computer.key", json!({"key":"DefinitelyNotAKey"}))
                .await
                .unwrap_err()
                .contains("unknown X11 keysym")
        );
        computer
            .perform("ssmcp.computer.type", json!({"text":"isolated input"}))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            while std::fs::read_to_string(&observed).unwrap_or_default() != "isolated input" {
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
        })
        .await
        .expect("private application must receive typed text");
        let shot = computer
            .perform("ssmcp.computer.screenshot", json!({"max_width":640}))
            .await
            .unwrap();
        assert_eq!(shot["structuredContent"]["desktop"], "isolated");
        assert_eq!(shot["structuredContent"]["width"], 640);
        assert_eq!(std::env::var_os("DISPLAY"), host_display);
        computer.shutdown().await.unwrap();
        assert!(!directory.exists());
        assert!(
            computer
                .perform("ssmcp.computer.move", json!({"x":0,"y":0}))
                .await
                .unwrap_err()
                .contains("closed")
        );
    }
}
