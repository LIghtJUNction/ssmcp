//! Private X11 desktops. All display selection is per child command.
use super::*;
use std::{
    fs::{DirBuilder, OpenOptions},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::Path,
};
use tokio::io::{AsyncBufReadExt, BufReader};

const MAX_APPS: usize = 16;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Start {
    #[serde(default = "width")]
    width: u32,
    #[serde(default = "height")]
    height: u32,
}
fn width() -> u32 {
    1280
}
fn height() -> u32 {
    800
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Launch {
    command: Vec<String>,
    cwd: Option<PathBuf>,
}

struct PrivateDirectory(PathBuf);
impl PrivateDirectory {
    fn new() -> Result<Self, String> {
        let mut random = [0u8; 16];
        getrandom::fill(&mut random).map_err(|e| e.to_string())?;
        let suffix: String = random.iter().map(|b| format!("{b:02x}")).collect();
        let path =
            std::env::temp_dir().join(format!("ssmcp-desktop-{}-{suffix}", std::process::id()));
        DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .map_err(|e| e.to_string())?;
        Ok(Self(path))
    }
}
impl Drop for PrivateDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub(super) struct Desktop {
    server: Option<Managed>,
    manager: Option<Managed>,
    apps: Vec<Managed>,
    directory: Option<PrivateDirectory>,
    environment: DisplayEnvironment,
    width: u32,
    height: u32,
}

impl Desktop {
    async fn start(root: &Path, options: &Start) -> Result<Self, String> {
        if !(320..=3840).contains(&options.width) || !(240..=2160).contains(&options.height) {
            return Err("isolated desktop dimensions must be 320–3840 by 240–2160".into());
        }
        let server = crate::program::executable("Xvfb")
            .map_err(|_| "Isolated desktop needs Xvfb. Install your distribution's Xvfb, xauth and xdotool packages; the shared desktop was not selected.".to_owned())?;
        crate::program::executable("xauth")?;
        crate::program::executable("xdotool")?;
        let directory = PrivateDirectory::new()?;
        let authority = directory.0.join("Xauthority");
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&authority)
            .map_err(|e| e.to_string())?;
        let runtime = directory.0.join("runtime");
        DirBuilder::new()
            .mode(0o700)
            .create(&runtime)
            .map_err(|e| e.to_string())?;
        let mut random = [0u8; 16];
        getrandom::fill(&mut random).map_err(|e| e.to_string())?;
        let cookie: String = random.iter().map(|b| format!("{b:02x}")).collect();
        // Xvfb loads the cookie from this private file independently of its
        // eventual display number. xauth's source input avoids an argv secret.
        write_authority(
            root,
            &authority,
            &format!("add :0 MIT-MAGIC-COOKIE-1 {cookie}\n"),
        )
        .await?;
        let mut command = tokio::process::Command::new(server);
        command
            .args([
                "-displayfd",
                "1",
                "-screen",
                "0",
                &format!("{}x{}x24", options.width, options.height),
                "-nolisten",
                "tcp",
                "-auth",
            ])
            .arg(&authority)
            .arg("-noreset")
            .current_dir(root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut server = Managed::spawn(&mut command).map_err(|e| format!("Xvfb: {e}"))?;
        let stdout = server
            .child
            .stdout
            .take()
            .ok_or("Xvfb display output unavailable")?;
        let mut desktop = Self {
            server: Some(server),
            manager: None,
            apps: Vec::new(),
            directory: Some(directory),
            environment: DisplayEnvironment {
                display: String::new(),
                authority,
                runtime,
                window_manager: false,
            },
            width: options.width,
            height: options.height,
        };
        let initialized = async {
            let mut display = String::new();
            let mut reader = BufReader::new(stdout.take(16));
            tokio::time::timeout(Duration::from_secs(8), reader.read_line(&mut display))
                .await
                .map_err(|_| "Xvfb did not announce a display within 8 seconds")?
                .map_err(|e| format!("Xvfb display read failed: {e}"))?;
            let display = display.trim();
            if display.is_empty()
                || display.len() > 5
                || !display.bytes().all(|b| b.is_ascii_digit())
            {
                return Err("Xvfb exited without announcing a valid private display".into());
            }
            desktop.environment.display = format!(":{display}");
            write_authority(
                root,
                &desktop.environment.authority,
                &format!("remove :0\nadd :{display} MIT-MAGIC-COOKIE-1 {cookie}\n"),
            )
            .await?;
            // Verify authentication and geometry before exposing the session.
            let geometry = run_helper_in(
                root,
                "xdotool",
                &["getdisplaygeometry".into()],
                None,
                1024,
                Duration::from_secs(3),
                Some(&desktop.environment),
            )
            .await?;
            if String::from_utf8_lossy(&geometry).trim()
                != format!("{} {}", options.width, options.height)
            {
                return Err("Xvfb geometry verification failed".into());
            }
            if let (Ok(manager), Ok(_)) = (
                crate::program::executable("openbox"),
                crate::program::executable("xprop"),
            ) {
                let mut command = tokio::process::Command::new(manager);
                desktop.environment.apply(command.as_std_mut());
                command
                    .arg("--sm-disable")
                    .current_dir(root)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());
                if let Ok(process) = Managed::spawn(&mut command) {
                    desktop.manager = Some(process);
                    let ready = tokio::time::timeout(Duration::from_secs(2), async {
                        loop {
                            if desktop
                                .manager
                                .as_mut()
                                .unwrap()
                                .child
                                .try_wait()
                                .map_err(|e| e.to_string())?
                                .is_some()
                            {
                                return Err("Openbox exited during startup".to_owned());
                            }
                            let property = run_helper_in(
                                root,
                                "xprop",
                                &["-root".into(), "_NET_SUPPORTING_WM_CHECK".into()],
                                None,
                                1024,
                                Duration::from_millis(500),
                                Some(&desktop.environment),
                            )
                            .await?;
                            if String::from_utf8_lossy(&property)
                                .split_whitespace()
                                .any(|part| part.starts_with("0x") && part != "0x0")
                            {
                                return Ok(());
                            }
                            tokio::time::sleep(Duration::from_millis(30)).await;
                        }
                    })
                    .await
                    .is_ok_and(|result| result.is_ok());
                    desktop.environment.window_manager = ready;
                    if !ready {
                        if let Some(process) = desktop.manager.as_mut() {
                            process.shutdown().await;
                        }
                        desktop.manager.take();
                    }
                }
            }
            Ok::<(), String>(())
        }
        .await;
        if let Err(error) = initialized {
            desktop.shutdown().await;
            return Err(error);
        }
        Ok(desktop)
    }

    pub(super) fn ensure_running(&mut self) -> Result<(), String> {
        let server = self.server.as_mut().ok_or("isolated desktop is stopped")?;
        if let Some(status) = server.child.try_wait().map_err(|e| e.to_string())? {
            return Err(format!(
                "Isolated desktop exited ({status}); stop it and start a new session. Shared desktop input was not selected."
            ));
        }
        Ok(())
    }

    fn info(&self) -> Value {
        json!({"desktop":"isolated","input_scope":"private_virtual_display",
            "display":self.environment.display,"width":self.width,"height":self.height,
            "window_manager":if self.environment.window_manager {"openbox"} else {"none"},
            "owned_applications":self.apps.len(),"shared_pointer":false})
    }

    async fn launch(&mut self, root: &Path, options: Launch) -> Result<Value, String> {
        self.ensure_running()?;
        if options.command.is_empty()
            || options.command.len() > 128
            || options.command[0].is_empty()
            || options
                .command
                .iter()
                .any(|v| v.contains('\0') || v.len() > 16384)
            || options.command.iter().map(String::len).sum::<usize>() > 65536
        {
            return Err("command must be 1–128 argv strings, each at most 16384 bytes, totaling at most 64 KiB without NUL".into());
        }
        let mut retained = Vec::new();
        for mut app in self.apps.drain(..) {
            if app.child.try_wait().map_err(|e| e.to_string())?.is_none() {
                retained.push(app);
            }
        }
        self.apps = retained;
        if self.apps.len() >= MAX_APPS {
            return Err("isolated desktop already owns 16 running applications".into());
        }
        let cwd = options.cwd.unwrap_or_else(|| root.to_owned());
        let cwd = if cwd.is_absolute() {
            cwd
        } else {
            root.join(cwd)
        };
        let cwd = cwd
            .canonicalize()
            .map_err(|e| format!("application cwd: {e}"))?;
        if !cwd.is_dir() {
            return Err("application cwd must be a directory".into());
        }
        let program = Path::new(&options.command[0]);
        let executable = if program.is_absolute() {
            if !crate::platform::is_executable(program) {
                return Err("application executable is not executable".into());
            }
            program.to_owned()
        } else if program.components().count() == 1 {
            crate::program::executable(&options.command[0])?
        } else {
            return Err("application executable must be an absolute path or a name in PATH".into());
        };
        let mut command = tokio::process::Command::new(executable);
        self.environment.apply(command.as_std_mut());
        command
            .args(&options.command[1..])
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let process = Managed::spawn(&mut command).map_err(|e| format!("application: {e}"))?;
        let pid = process.child.id();
        self.apps.push(process);
        Ok(
            json!({"started":true,"supervisor_pid":pid,"desktop":"isolated",
            "display":self.environment.display,"input_scope":"private_virtual_display"}),
        )
    }

    pub(super) async fn shutdown(&mut self) {
        // Start all termination together, and retain ownership across awaits so
        // Drop can finish reaping if the caller cancels this shutdown.
        for process in &mut self.apps {
            process.terminate();
        }
        if let Some(process) = self.manager.as_mut() {
            process.terminate();
        }
        if let Some(process) = self.server.as_mut() {
            process.terminate();
        }
        for process in &mut self.apps {
            process.shutdown().await;
        }
        self.apps.clear();
        if let Some(process) = self.manager.as_mut() {
            process.shutdown().await;
        }
        self.manager.take();
        if let Some(process) = self.server.as_mut() {
            process.shutdown().await;
        }
        self.server.take();
        self.directory.take();
    }
}

impl Drop for Desktop {
    fn drop(&mut self) {
        let mut processes: Vec<_> = self.apps.drain(..).collect();
        processes.extend(self.manager.take());
        processes.extend(self.server.take());
        if processes.is_empty() {
            return;
        }
        for process in &mut processes {
            process.terminate();
        }
        let directory = self.directory.take();
        // Cancellation must not delete Xauthority before all owned applications
        // and the server have exited. The process guardian owns descendants.
        std::thread::spawn(move || {
            for mut process in processes {
                while matches!(process.child.try_wait(), Ok(None)) {
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
            drop(directory);
        });
    }
}

async fn write_authority(root: &Path, authority: &Path, instructions: &str) -> Result<(), String> {
    run_helper(
        root,
        "xauth",
        &[
            "-f".into(),
            authority.to_string_lossy().into_owned(),
            "-q".into(),
            "source".into(),
            "-".into(),
        ],
        Some(instructions.as_bytes()),
        1024,
        Duration::from_secs(3),
    )
    .await
    .map(|_| ())
}

impl Computer {
    pub(super) async fn session_operation(&self, name: &str, args: Value) -> Result<Value, String> {
        let mut active = self.isolated.lock().await;
        match name {
            "ssmcp.computer.session_start" => {
                let options: Start = parse(args)?;
                if let Some(existing) = active.as_mut() {
                    existing.desktop.ensure_running()?;
                    if (existing.desktop.width, existing.desktop.height)
                        != (options.width, options.height)
                    {
                        return Err("An isolated desktop is already active with different dimensions; stop it before changing its size.".into());
                    }
                    return Ok(result(existing.desktop.info()));
                }
                let desktop = Desktop::start(&self.root, &options).await?;
                let mut computer = Computer::new(self.root.clone())?;
                computer.session = Session::X11;
                computer.display = Some(desktop.environment.clone());
                let info = desktop.info();
                *active = Some(IsolatedComputer {
                    desktop,
                    computer: Box::new(computer),
                });
                Ok(result(info))
            }
            "ssmcp.computer.session_stop" => {
                parse::<Empty>(args)?;
                if let Some(mut desktop) = active.take() {
                    let release = desktop.computer.releases.wait(RELEASE_WAIT_TIMEOUT).await;
                    desktop.desktop.shutdown().await;
                    release?;
                }
                Ok(result(
                    json!({"stopped":true,"desktop":"isolated","shared_input_enabled":false}),
                ))
            }
            "ssmcp.computer.launch" => {
                let options: Launch = parse(args)?;
                let active = active.as_mut().ok_or("Start an isolated desktop with computer.session_start before launching applications")?;
                active.desktop.launch(&self.root, options).await.map(result)
            }
            _ => Err("unknown isolated desktop operation".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn isolation_environment_is_per_command_and_authority_stays_private() {
        use std::os::unix::fs::PermissionsExt;
        let directory = PrivateDirectory::new().unwrap();
        assert_eq!(
            std::fs::metadata(&directory.0)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        let environment = DisplayEnvironment {
            display: ":54321".into(),
            authority: directory.0.join("auth"),
            runtime: directory.0.join("runtime"),
            window_manager: false,
        };
        let original = std::env::var_os("DISPLAY");
        let mut command = std::process::Command::new("true");
        environment.apply(&mut command);
        let env: std::collections::HashMap<_, _> = command.get_envs().collect();
        assert_eq!(
            env[std::ffi::OsStr::new("DISPLAY")],
            Some(std::ffi::OsStr::new(":54321"))
        );
        assert_eq!(env[std::ffi::OsStr::new("WAYLAND_DISPLAY")], None);
        assert_eq!(std::env::var_os("DISPLAY"), original);
    }

    #[tokio::test]
    async fn default_input_never_reaches_shared_desktop() {
        let computer = Computer::new(std::env::temp_dir()).unwrap();
        for name in ["focus", "move", "click", "drag", "key", "type", "scroll"] {
            let error = computer
                .perform(&format!("ssmcp.computer.{name}"), json!({}))
                .await
                .unwrap_err();
            assert!(error.contains("isolated session"), "{name}: {error}");
        }
        assert!(
            computer
                .perform("ssmcp.computer.launch", json!({"command":["true"]}))
                .await
                .unwrap_err()
                .contains("Start an isolated desktop")
        );
        assert!(
            computer
                .perform("ssmcp.computer.session_start", json!({"width":0}))
                .await
                .unwrap_err()
                .contains("dimensions")
        );
    }

    #[tokio::test]
    async fn shared_release_failure_does_not_block_private_session_cleanup() {
        let computer = Computer::new(std::env::temp_dir()).unwrap();
        computer.releases.state.lock().unwrap().failure =
            Some("shared input release failed".into());
        let stopped = computer
            .perform("ssmcp.computer.session_stop", json!({}))
            .await
            .unwrap();
        assert_eq!(stopped["structuredContent"]["stopped"], true);
        let error = computer
            .perform("ssmcp.computer.status", json!({"desktop":"shared"}))
            .await
            .unwrap_err();
        assert_eq!(error, "shared input release failed");
    }

    #[tokio::test]
    #[ignore = "requires real Xvfb, xauth, xdotool and ImageMagick"]
    async fn real_isolated_desktop_input_capture_and_cleanup() {
        let original = std::env::var_os("DISPLAY");
        let computer = Computer::new(std::env::temp_dir()).unwrap();
        let started = computer
            .perform(
                "ssmcp.computer.session_start",
                json!({"width":640,"height":480}),
            )
            .await
            .unwrap();
        let display = started["structuredContent"]["display"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(started["structuredContent"]["shared_pointer"], false);
        let again = computer
            .perform(
                "ssmcp.computer.session_start",
                json!({"width":640,"height":480}),
            )
            .await
            .unwrap();
        assert_eq!(again["structuredContent"]["display"], display);
        let directory = computer
            .isolated
            .lock()
            .await
            .as_ref()
            .unwrap()
            .desktop
            .directory
            .as_ref()
            .unwrap()
            .0
            .clone();
        let fixture = r#"
import ctypes, pathlib, sys, time
x = ctypes.CDLL('libX11.so.6')
x.XOpenDisplay.restype = ctypes.c_void_p
d = x.XOpenDisplay(None)
assert d
x.XDefaultRootWindow.argtypes = [ctypes.c_void_p]
x.XDefaultRootWindow.restype = ctypes.c_ulong
x.XCreateSimpleWindow.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.c_int, ctypes.c_int, ctypes.c_uint, ctypes.c_uint, ctypes.c_uint, ctypes.c_ulong, ctypes.c_ulong]
x.XCreateSimpleWindow.restype = ctypes.c_ulong
w = x.XCreateSimpleWindow(d, x.XDefaultRootWindow(d), 20, 20, 300, 200, 1, 0, 0xeeeeee)
x.XStoreName.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.c_char_p]
x.XStoreName(d, w, b'SSMCP isolated fixture')
x.XSelectInput.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.c_long]
x.XSelectInput(d, w, 1 | 4)
x.XMapWindow.argtypes = [ctypes.c_void_p, ctypes.c_ulong]
x.XMapWindow(d, w)
x.XFlush.argtypes = [ctypes.c_void_p]
x.XFlush(d)
x.XNextEvent.argtypes = [ctypes.c_void_p, ctypes.c_void_p]
event = (ctypes.c_long * 24)()
while True:
    x.XNextEvent(d, ctypes.byref(event))
    pathlib.Path(sys.argv[1], 'event-' + str(event[0])).write_text('received')
"#;
        let launched = computer
            .perform(
                "ssmcp.computer.launch",
                json!({"command":["python3","-c",fixture,directory.to_string_lossy()]}),
            )
            .await
            .unwrap();
        let pid = launched["structuredContent"]["supervisor_pid"]
            .as_u64()
            .unwrap();
        let window = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let windows = computer
                    .perform("ssmcp.computer.windows", json!({}))
                    .await
                    .unwrap();
                if let Some(window) = windows["structuredContent"]["windows"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|window| window["title"] == "SSMCP isolated fixture")
                {
                    break window["window_id"].clone();
                }
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
        })
        .await
        .unwrap();
        computer
            .perform("ssmcp.computer.focus", json!({"window_id":window}))
            .await
            .unwrap();
        computer
            .perform("ssmcp.computer.key", json!({"key":"a"}))
            .await
            .unwrap();
        computer
            .perform("ssmcp.computer.move", json!({"x":120,"y":90}))
            .await
            .unwrap();
        computer
            .perform("ssmcp.computer.click", json!({}))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while !directory.join("event-2").exists() || !directory.join("event-4").exists() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        let image = computer
            .perform("ssmcp.computer.screenshot", json!({}))
            .await
            .unwrap();
        assert_eq!(image["structuredContent"]["desktop"], "isolated");
        assert!(
            image["content"]
                .as_array()
                .unwrap()
                .iter()
                .any(|value| value["type"] == "image")
        );
        assert!(directory.exists());
        computer
            .perform("ssmcp.computer.session_stop", json!({}))
            .await
            .unwrap();
        assert!(!directory.exists());
        assert!(!Path::new(&format!("/proc/{pid}")).exists());
        assert!(
            !Path::new(&format!(
                "/tmp/.X11-unix/X{}",
                display.trim_start_matches(':')
            ))
            .exists()
        );
        assert_eq!(std::env::var_os("DISPLAY"), original);
        assert!(
            computer
                .perform("ssmcp.computer.click", json!({}))
                .await
                .unwrap_err()
                .contains("isolated session")
        );
        computer
            .perform(
                "ssmcp.computer.session_start",
                json!({"width":640,"height":480}),
            )
            .await
            .unwrap();
        {
            let mut active = computer.isolated.lock().await;
            active
                .as_mut()
                .unwrap()
                .desktop
                .server
                .as_mut()
                .unwrap()
                .shutdown()
                .await;
        }
        assert!(
            computer
                .perform("ssmcp.computer.click", json!({}))
                .await
                .unwrap_err()
                .contains("exited")
        );
        computer
            .perform("ssmcp.computer.session_stop", json!({}))
            .await
            .unwrap();
    }
}
