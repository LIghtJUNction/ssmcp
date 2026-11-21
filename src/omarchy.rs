//! Omarchy desktop integration through bounded, noninteractive system commands.
//! Arguments are validated values; no user text is evaluated as a shell command.
use crate::{
    backend::{process::Managed, result},
    computer::host_control::{HostControl, Permit},
};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Read,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{io::AsyncReadExt, sync::Mutex};

const OUTPUT_LIMIT: usize = 128 * 1024;
const RESULT_LIMIT: usize = 24 * 1024;
const SCAN_LIMIT: usize = 2048;
const ITEM_LIMIT: usize = 200;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(15);

pub struct Omarchy {
    root: PathBuf,
    home: Option<PathBuf>,
    installation: Option<PathBuf>,
    path: Vec<PathBuf>,
    application_dirs: Vec<PathBuf>,
    hyprland: bool,
    timeout: Duration,
    closed: AtomicBool,
    serial: Mutex<()>,
    observed_apps: Mutex<BTreeSet<String>>,
    control: Arc<HostControl>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Name {
    name: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Background {
    path: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AudioSet {
    target: String,
    percent: Option<u8>,
    muted: Option<bool>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Brightness {
    monitor: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BrightnessSet {
    monitor: Option<String>,
    percent: u8,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ToggleSet {
    feature: String,
    enabled: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Workspace {
    workspace_id: u32,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WindowAction {
    window_id: String,
    action: String,
    enabled: Option<bool>,
    workspace_id: Option<u32>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Apps {
    #[serde(default)]
    query: String,
    #[serde(default = "default_app_limit")]
    limit: usize,
}
fn default_app_limit() -> usize {
    40
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AppLaunch {
    app_id: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MenuRequest {
    action: String,
    route: Option<String>,
}

struct CommandOutput {
    code: i32,
    stdout: String,
    stderr: String,
}
#[derive(Clone)]
struct App {
    id: String,
    name: String,
    icon: Option<String>,
}

impl Omarchy {
    #[cfg(test)]
    pub fn new(root: PathBuf) -> Result<Self, String> {
        Self::with_control(root, Arc::new(HostControl::default()))
    }

    pub(crate) fn with_control(root: PathBuf, control: Arc<HostControl>) -> Result<Self, String> {
        let root = root.canonicalize().map_err(|e| e.to_string())?;
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute());
        let mut candidates = Vec::new();
        if let Some(p) = std::env::var_os("OMARCHY_PATH")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
        {
            candidates.push(p);
        }
        candidates.push(PathBuf::from("/usr/share/omarchy"));
        if let Some(h) = &home {
            candidates.push(h.join(".local/share/omarchy"));
        }
        let installation = candidates.into_iter().find(|p| p.is_dir());
        let mut path: Vec<PathBuf> = std::env::var_os("PATH")
            .map(|v| std::env::split_paths(&v).collect())
            .unwrap_or_default();
        if let Some(p) = &installation {
            path.push(p.join("bin"));
        }
        for p in ["/usr/local/bin", "/usr/bin", "/bin"] {
            path.push(p.into());
        }
        path.retain(|p| p.is_absolute());
        path.dedup();
        let mut application_dirs = Vec::new();
        if let Some(p) = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
        {
            application_dirs.push(p.join("applications"));
        } else if let Some(h) = &home {
            application_dirs.push(h.join(".local/share/applications"));
        }
        let data_dirs = std::env::var_os("XDG_DATA_DIRS")
            .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
        application_dirs.extend(
            std::env::split_paths(&data_dirs)
                .filter(|p| p.is_absolute())
                .take(16)
                .map(|p| p.join("applications")),
        );
        Ok(Self {
            root,
            home,
            installation,
            path,
            application_dirs,
            hyprland: std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some(),
            timeout: COMMAND_TIMEOUT,
            closed: AtomicBool::new(false),
            serial: Mutex::new(()),
            observed_apps: Mutex::new(BTreeSet::new()),
            control,
        })
    }

    pub fn tools() -> Vec<Value> {
        let label = json!({"type":"string","minLength":1,"maxLength":128});
        let monitor = json!({"type":"string","minLength":1,"maxLength":128});
        let workspace = json!({"type":"integer","minimum":1,"maximum":9999});
        vec![
            tool(
                "status",
                "Inspect Omarchy installation and command capabilities without changing the desktop.",
                json!({}),
                &[],
                true,
            ),
            tool(
                "themes",
                "List installed Omarchy themes and the selected theme.",
                json!({}),
                &[],
                true,
            ),
            tool(
                "theme_set",
                "Select an installed theme by its exact name from omarchy.themes. Verify selected-theme state; app retint hooks may finish separately.",
                json!({"name":label}),
                &["name"],
                false,
            ),
            tool(
                "backgrounds",
                "List bounded background media for the current theme and the current background path.",
                json!({}),
                &[],
                true,
            ),
            tool(
                "background_set",
                "Select a background path returned by omarchy.backgrounds. Verify the selection; do not infer rendered pixels.",
                json!({"path":{"type":"string","minLength":1,"maxLength":4096}}),
                &["path"],
                false,
            ),
            tool(
                "fonts",
                "List installed monospace fonts and the selected font.",
                json!({}),
                &[],
                true,
            ),
            tool(
                "font_set",
                "Set an installed monospace font by its exact name from omarchy.fonts and verify fontconfig state.",
                json!({"name":label}),
                &["name"],
                false,
            ),
            tool(
                "audio",
                "Read output volume/mute through Omarchy's physical-sink resolver and default microphone state.",
                json!({}),
                &[],
                true,
            ),
            tool(
                "audio_set",
                "Set an explicit output/input volume percentage or mute state and return verified resulting state.",
                json!({"target":{"type":"string","enum":["output","input"]},"percent":{"type":"integer","minimum":0,"maximum":100},"muted":{"type":"boolean"}}),
                &["target"],
                false,
            ),
            tool(
                "brightness",
                "Read brightness of the focused display or an exact monitor name from computer.monitors.",
                json!({"monitor":monitor}),
                &[],
                true,
            ),
            tool(
                "brightness_set",
                "Set display brightness to 1–100 percent and read back the actual brightness.",
                json!({"monitor":monitor,"percent":{"type":"integer","minimum":1,"maximum":100}}),
                &["percent"],
                false,
            ),
            tool(
                "toggles",
                "Read nightlight, stay-awake, bar visibility, and animation settings. Unsupported controls include diagnostics.",
                json!({}),
                &[],
                true,
            ),
            tool(
                "toggle_set",
                "Set a desired boolean value for a supported Omarchy setting and verify its state.",
                json!({"feature":{"type":"string","enum":["nightlight","stay_awake","bar_visible","animations"]},"enabled":{"type":"boolean"}}),
                &["feature", "enabled"],
                false,
            ),
            tool(
                "workspaces",
                "Read bounded Hyprland workspaces and the active workspace.",
                json!({}),
                &[],
                true,
            ),
            tool(
                "workspace_switch",
                "Activate workspace 1–9999. Skip if already active and verify the resulting active workspace.",
                json!({"workspace_id":workspace}),
                &["workspace_id"],
                false,
            ),
            tool(
                "window_action",
                "Set a current window's floating/fullscreen state, move it to a numeric workspace, or request graceful close. Requires window_id from computer.windows; does not focus another window first.",
                json!({"window_id":{"type":"string","pattern":"^0x[0-9a-fA-F]{1,16}$"},"action":{"type":"string","enum":["floating","fullscreen","move_to_workspace","close"]},"enabled":{"type":"boolean"},"workspace_id":workspace}),
                &["window_id", "action"],
                false,
            ),
            tool(
                "apps",
                "Search installed visible desktop applications. Returned app IDs can be passed to app_launch; scans and output are bounded.",
                json!({"query":{"type":"string","maxLength":128},"limit":{"type":"integer","minimum":1,"maximum":100,"default":40}}),
                &[],
                true,
            ),
            tool(
                "app_launch",
                "Request launching an app ID from the latest apps result through the desktop launcher. Acceptance is not proof a window appeared.",
                json!({"app_id":{"type":"string","minLength":1,"maxLength":256}}),
                &["app_id"],
                false,
            ),
            tool(
                "menu",
                "Request showing or closing Omarchy's native menu. Returns request acceptance without claiming visibility.",
                json!({"action":{"type":"string","enum":["show","close"]},"route":{"type":"string","enum":["root","appearance","appearance.theme","appearance.font","setup","system"]}}),
                &["action"],
                false,
            ),
            tool(
                "battery",
                "Read Omarchy battery percentage, charge state, rate and capacity; no battery settings are changed.",
                json!({}),
                &[],
                true,
            ),
            tool(
                "network",
                "Read the active route's network type and link status without scanning, sending probes, or returning credentials.",
                json!({}),
                &[],
                true,
            ),
        ]
    }

    pub async fn shutdown(&self) {
        self.closed.store(true, Ordering::SeqCst);
        let _guard = self.serial.lock().await;
        self.observed_apps.lock().await.clear();
    }

    pub async fn perform(&self, name: &str, args: Value) -> Result<Value, String> {
        let name = name
            .strip_prefix("ssmcp.omarchy.")
            .ok_or("not an Omarchy tool")?;
        let operation = self.perform_inner(name, args);
        if matches!(
            name,
            "status"
                | "themes"
                | "backgrounds"
                | "fonts"
                | "audio"
                | "brightness"
                | "toggles"
                | "workspaces"
                | "apps"
                | "battery"
                | "network"
        ) {
            operation.await
        } else {
            self.control
                .permit()?
                .serialized()
                .await?
                .run(operation)
                .await
        }
    }

    async fn perform_inner(&self, name: &str, args: Value) -> Result<Value, String> {
        let _guard = self.serial.lock().await;
        if self.closed.load(Ordering::SeqCst) {
            return Err("Omarchy control is closed".into());
        }
        Permit::check_current()?;
        if name == "status" {
            parse::<Empty>(args)?;
            let mut status = self.status().await;
            status["computer_use"] = self.control.status();
            return finish(status);
        }
        if !self.installed() {
            return Err("Omarchy system primitives require a Linux Omarchy installation; use omarchy.status for diagnostics".into());
        }
        let value = match name {
            "themes" => {
                parse::<Empty>(args)?;
                self.named_list("theme").await?
            }
            "theme_set" => {
                let v: Name = parse(args)?;
                self.set_name("theme", &v.name).await?
            }
            "backgrounds" => {
                parse::<Empty>(args)?;
                self.backgrounds()?
            }
            "background_set" => {
                let v: Background = parse(args)?;
                self.set_background(&v.path).await?
            }
            "fonts" => {
                parse::<Empty>(args)?;
                self.named_list("font").await?
            }
            "font_set" => {
                let v: Name = parse(args)?;
                self.set_name("font", &v.name).await?
            }
            "audio" => {
                parse::<Empty>(args)?;
                self.audio().await
            }
            "audio_set" => self.set_audio(parse(args)?).await?,
            "brightness" => {
                let v: Brightness = parse(args)?;
                self.brightness(v.monitor.as_deref()).await?
            }
            "brightness_set" => self.set_brightness(parse(args)?).await?,
            "toggles" => {
                parse::<Empty>(args)?;
                self.toggles().await
            }
            "toggle_set" => self.set_toggle(parse(args)?).await?,
            "workspaces" => {
                parse::<Empty>(args)?;
                self.workspaces().await?
            }
            "workspace_switch" => {
                let v: Workspace = parse(args)?;
                self.workspace_switch(v.workspace_id).await?
            }
            "window_action" => self.window_action(parse(args)?).await?,
            "apps" => self.apps(parse(args)?).await?,
            "app_launch" => {
                let v: AppLaunch = parse(args)?;
                self.app_launch(&v.app_id).await?
            }
            "menu" => self.menu(parse(args)?).await?,
            "battery" => {
                parse::<Empty>(args)?;
                self.battery().await?
            }
            "network" => {
                parse::<Empty>(args)?;
                self.network().await?
            }
            _ => return Err(format!("unknown Omarchy tool: {name}")),
        };
        let output = finish(value)?;
        if name == "apps" {
            *self.observed_apps.lock().await = output["structuredContent"]["apps"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|a| a["app_id"].as_str().map(str::to_owned))
                .collect();
        }
        Ok(output)
    }

    fn executable(&self, name: &str) -> Result<PathBuf, String> {
        self.path
            .iter()
            .map(|d| d.join(name))
            .find(|p| executable_file(p))
            .ok_or_else(|| format!("{name} is not installed or executable"))
    }
    fn installed(&self) -> bool {
        cfg!(target_os = "linux")
            && self.installation.is_some()
            && self.executable("omarchy-version").is_ok()
    }
    fn script_has(&self, name: &str, needle: &str) -> bool {
        self.executable(name)
            .ok()
            .and_then(|p| read_bounded(&p, 64 * 1024).ok())
            .is_some_and(|s| s.contains(needle))
    }
    async fn status(&self) -> Value {
        let groups: &[(&str, &[&str])] = &[
            (
                "themes",
                &[
                    "omarchy-theme-list",
                    "omarchy-theme-current",
                    "omarchy-theme-set",
                ],
            ),
            ("backgrounds", &["omarchy-theme-bg-set"]),
            (
                "fonts",
                &[
                    "omarchy-font-list",
                    "omarchy-font-current",
                    "omarchy-font-set",
                ],
            ),
            ("audio_output", &["omarchy-audio-output-sink", "pactl"]),
            ("audio_input", &["wpctl"]),
            ("brightness", &["omarchy-brightness-display", "hyprctl"]),
            ("workspaces", &["hyprctl"]),
            ("apps", &["gtk-launch", "systemd-run"]),
            ("menu", &["omarchy-menu"]),
            ("battery", &["omarchy-battery-status"]),
            ("network", &["omarchy-network-status"]),
        ];
        let installed = self.installed();
        let mut capabilities = Map::new();
        for (name, programs) in groups {
            let missing: Vec<_> = programs
                .iter()
                .filter(|p| self.executable(p).is_err())
                .copied()
                .collect();
            capabilities.insert((*name).into(),json!({"installed":installed&&missing.is_empty(),"missing_commands":missing,"runtime_probed":false}));
        }
        let version = if installed {
            self.text("omarchy-version", &[])
                .await
                .ok()
                .map(|s| bounded(s.trim(), 128))
        } else {
            None
        };
        json!({"platform":std::env::consts::OS,"available":installed,"version":version,"installation":self.installation,
            "session":if self.hyprland {"hyprland"}else{"other"},"native":true,"capabilities":capabilities,
            "reason":if installed {None}else{Some("A Linux Omarchy installation with official omarchy-* commands is required.")},
            "limits":{"result_bytes":RESULT_LIMIT,"scan_entries":SCAN_LIMIT,"list_items":ITEM_LIMIT},
            "note":"Installed helpers are not a hardware/permission probe. Individual getters report actual availability."})
    }

    async fn run(&self, program: &str, args: &[&str]) -> Result<CommandOutput, String> {
        let exe = self.executable(program)?;
        let mut command = tokio::process::Command::new(exe);
        command
            .args(args)
            .current_dir(&self.root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("LC_ALL", "C");
        if let Ok(path) = std::env::join_paths(&self.path) {
            command.env("PATH", path);
        }
        if let Some(p) = &self.installation {
            command.env("OMARCHY_PATH", p);
        }
        if program == "omarchy-theme-set" {
            command.env("OMARCHY_THEME_WAIT", "1");
        }
        Permit::check_current()?;
        let mut process =
            Managed::spawn(&mut command).map_err(|e| format!("could not start {program}: {e}"))?;
        let stdout = process
            .child
            .stdout
            .take()
            .ok_or("missing command stdout")?;
        let stderr = process
            .child
            .stderr
            .take()
            .ok_or("missing command stderr")?;
        let collect = async {
            let output = async {
                let mut bytes = Vec::new();
                stdout
                    .take((OUTPUT_LIMIT + 1) as u64)
                    .read_to_end(&mut bytes)
                    .await
                    .map_err(|e| e.to_string())?;
                if bytes.len() > OUTPUT_LIMIT {
                    return Err("command stdout exceeded 128 KiB".to_owned());
                }
                String::from_utf8(bytes).map_err(|e| e.to_string())
            };
            let errors = async {
                let mut bytes = Vec::new();
                stderr
                    .take((OUTPUT_LIMIT + 1) as u64)
                    .read_to_end(&mut bytes)
                    .await
                    .map_err(|e| e.to_string())?;
                if bytes.len() > OUTPUT_LIMIT {
                    return Err("command stderr exceeded 128 KiB".to_owned());
                }
                String::from_utf8(bytes).map_err(|e| e.to_string())
            };
            let wait = async { process.wait().await.map_err(|e| e.to_string()) };
            let (stdout, stderr, status) = tokio::try_join!(output, errors, wait)?;
            Ok::<_, String>(CommandOutput {
                code: status.code().unwrap_or(-1),
                stdout,
                stderr,
            })
        };
        match tokio::time::timeout(self.timeout, collect).await {
            Ok(Ok(output)) => Ok(output),
            Ok(Err(error)) => {
                process.shutdown().await;
                Err(format!("{program}: {error}"))
            }
            Err(_) => {
                process.shutdown().await;
                Err(format!(
                    "{program} timed out after 15 seconds; any completed state change must be checked before retrying"
                ))
            }
        }
    }
    async fn success(&self, program: &str, args: &[&str]) -> Result<CommandOutput, String> {
        let output = self.run(program, args).await?;
        if output.code != 0 {
            return Err(format!(
                "{program} exited {}: {}",
                output.code,
                bounded(output.stderr.trim(), 1500)
            ));
        }
        Ok(output)
    }
    async fn text(&self, program: &str, args: &[&str]) -> Result<String, String> {
        Ok(self.success(program, args).await?.stdout)
    }
    async fn hypr(&self, query: &str) -> Result<Value, String> {
        if !self.hyprland {
            return Err("An active Hyprland session is required".into());
        }
        serde_json::from_str(&self.text("hyprctl", &["-j", query]).await?)
            .map_err(|e| format!("invalid Hyprland {query} response: {e}"))
    }
    async fn has_lua(&self) -> bool {
        self.text("hyprctl", &["repl", "return type(hl.dsp.focus)"])
            .await
            .is_ok_and(|s| s.trim() == "function")
    }
    async fn dispatch(&self, lua: &str, legacy: &[&str]) -> Result<(), String> {
        let output = if self.has_lua().await {
            self.text("hyprctl", &["eval", lua]).await?
        } else {
            self.text("hyprctl", legacy).await?
        };
        if !output.trim().is_empty() && output.trim() != "ok" {
            return Err(format!(
                "Hyprland rejected the action: {}",
                bounded(output.trim(), 1000)
            ));
        }
        Ok(())
    }

    async fn named_list(&self, kind: &str) -> Result<Value, String> {
        let list_program = format!("omarchy-{kind}-list");
        let current_program = format!("omarchy-{kind}-current");
        let list = self.success(&list_program, &[]).await?;
        let (names, truncated) = parse_names(&list.stdout);
        let current = self.text(&current_program, &[]).await?;
        Ok(
            json!({"names":names,"current":bounded(current.trim(),128),"truncated":truncated,"warnings":bounded(list.stderr.trim(),1000)}),
        )
    }
    async fn set_name(&self, kind: &str, name: &str) -> Result<Value, String> {
        validate_label(name)?;
        let choices = self.named_list(kind).await?;
        if !choices["names"]
            .as_array()
            .is_some_and(|a| a.iter().any(|n| n.as_str() == Some(name)))
        {
            return Err(format!(
                "name is not in omarchy.{}; refresh the list",
                if kind == "theme" { "themes" } else { "fonts" }
            ));
        }
        if choices["current"].as_str() == Some(name) {
            return Ok(json!({"changed":false,"current":name,"verified":true}));
        }
        let output = self
            .success(&format!("omarchy-{kind}-set"), &[name])
            .await?;
        let current = self.text(&format!("omarchy-{kind}-current"), &[]).await?;
        if current.trim() != name {
            return Err(format!(
                "{kind} command completed but selected state is '{}'; expected '{}'",
                bounded(current.trim(), 128),
                name
            ));
        }
        Ok(
            json!({"changed":true,"current":name,"verified":true,"verification":if kind=="theme" {"selected_theme"}else{"fontconfig_family"},"warnings":bounded(output.stderr.trim(),1500)}),
        )
    }
    fn state_candidates(&self, suffix: &str) -> Vec<PathBuf> {
        self.home
            .as_ref()
            .map(|h| {
                vec![
                    h.join(".local/state/omarchy").join(suffix),
                    h.join(".config/omarchy").join(suffix),
                ]
            })
            .unwrap_or_default()
    }
    fn current_background(&self) -> Option<PathBuf> {
        self.state_candidates("current/background")
            .into_iter()
            .find_map(|p| p.canonicalize().ok().filter(|p| p.is_file()))
    }
    fn background_paths(&self) -> Result<(Vec<PathBuf>, bool), String> {
        let mut dirs = self.state_candidates("current/theme/backgrounds");
        if let Some(h) = &self.home {
            let slug = self
                .state_candidates("current/theme.name")
                .iter()
                .find_map(|p| read_bounded(p, 512).ok());
            if let Some(slug) = slug.map(|s| s.trim().to_owned()).filter(|s| safe_slug(s)) {
                dirs.push(h.join(".config/omarchy/backgrounds").join(slug));
            }
        }
        let mut files = BTreeSet::new();
        let mut scanned = 0;
        let mut truncated = false;
        for dir in dirs {
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            for entry in entries {
                scanned += 1;
                if scanned > SCAN_LIMIT || files.len() >= ITEM_LIMIT {
                    truncated = true;
                    break;
                }
                let Ok(entry) = entry else { continue };
                let path = entry.path();
                if !media_extension(&path) {
                    continue;
                }
                if let Ok(p) = path.canonicalize()
                    && p.is_file()
                    && p.to_str()
                        .is_some_and(|s| s.len() <= 4096 && !s.chars().any(char::is_control))
                {
                    files.insert(p);
                }
            }
            if truncated {
                break;
            }
        }
        Ok((files.into_iter().collect(), truncated))
    }
    fn backgrounds(&self) -> Result<Value, String> {
        let (paths, truncated) = self.background_paths()?;
        let items: Vec<_> = paths
            .into_iter()
            .map(|p| json!({"name":p.file_name().unwrap_or_default().to_string_lossy(),"path":p}))
            .collect();
        Ok(json!({"backgrounds":items,"current":self.current_background(),"truncated":truncated}))
    }
    async fn set_background(&self, path: &str) -> Result<Value, String> {
        if path.len() > 4096 || path.chars().any(char::is_control) {
            return Err("invalid background path".into());
        }
        let p = PathBuf::from(path);
        if !self.background_paths()?.0.contains(&p) {
            return Err("path is not in omarchy.backgrounds; refresh the list".into());
        }
        if self.current_background().as_ref() == Some(&p) {
            return Ok(json!({"changed":false,"current":p,"verified":true}));
        }
        let output = self.success("omarchy-theme-bg-set", &[path]).await?;
        let current = self.current_background();
        if current.as_ref() != Some(&p) {
            return Err("background command completed but selected background does not match; refresh omarchy.backgrounds".into());
        }
        Ok(
            json!({"changed":true,"current":current,"verified":true,"verification":"selected_background","warnings":bounded(output.stderr.trim(),1500)}),
        )
    }

    async fn output_sink(&self) -> Result<String, String> {
        let sink = self.text("omarchy-audio-output-sink", &[]).await?;
        let sink = sink.trim();
        if sink.is_empty()
            || sink.len() > 256
            || sink.starts_with('-')
            || sink.chars().any(char::is_control)
        {
            return Err("Omarchy returned an invalid output sink".into());
        }
        Ok(sink.to_owned())
    }
    async fn audio_target(&self, target: &str) -> Result<Value, String> {
        match target {
            "output" => {
                let sink = self.output_sink().await?;
                let volume = self.text("pactl", &["get-sink-volume", &sink]).await?;
                let muted = self.text("pactl", &["get-sink-mute", &sink]).await?;
                let percent = parse_percent(&volume).ok_or("could not parse output volume")?;
                let muted = match muted.trim().rsplit_once(':').map(|(_, s)| s.trim()) {
                    Some("yes") => true,
                    Some("no") => false,
                    _ => return Err("could not parse output mute state".into()),
                };
                Ok(json!({"available":true,"percent":percent,"muted":muted}))
            }
            "input" => {
                let text = self
                    .text("wpctl", &["get-volume", "@DEFAULT_AUDIO_SOURCE@"])
                    .await?;
                let (percent, muted) = parse_wpctl(&text)?;
                Ok(json!({"available":true,"percent":percent,"muted":muted}))
            }
            _ => Err("target must be output or input".into()),
        }
    }
    async fn audio(&self) -> Value {
        let (output, input) = tokio::join!(self.audio_target("output"), self.audio_target("input"));
        json!({"output":observed(output),"input":observed(input)})
    }
    async fn set_audio(&self, v: AudioSet) -> Result<Value, String> {
        if v.target != "output" && v.target != "input" {
            return Err("target must be output or input".into());
        }
        if v.percent.is_none() && v.muted.is_none() {
            return Err("supply percent or muted".into());
        }
        if v.percent.is_some_and(|p| p > 100) {
            return Err("percent must be between 0 and 100".into());
        }
        let before = self.audio_target(&v.target).await?;
        if v.target == "output" {
            let sink = self.output_sink().await?;
            if let Some(p) = v
                .percent
                .filter(|p| before["percent"].as_u64() != Some(u64::from(*p)))
            {
                self.success("pactl", &["set-sink-volume", &sink, &format!("{p}%")])
                    .await?;
            }
            if let Some(m) = v.muted.filter(|m| before["muted"].as_bool() != Some(*m)) {
                self.success(
                    "pactl",
                    &["set-sink-mute", &sink, if m { "1" } else { "0" }],
                )
                .await?;
            }
        } else {
            if let Some(p) = v
                .percent
                .filter(|p| before["percent"].as_u64() != Some(u64::from(*p)))
            {
                self.success(
                    "wpctl",
                    &["set-volume", "@DEFAULT_AUDIO_SOURCE@", &format!("{p}%")],
                )
                .await?;
            }
            if let Some(m) = v.muted.filter(|m| before["muted"].as_bool() != Some(*m)) {
                if self.executable("omarchy-audio-input-mute").is_ok() {
                    self.success("omarchy-audio-input-mute", &[]).await?;
                } else {
                    self.success(
                        "wpctl",
                        &[
                            "set-mute",
                            "@DEFAULT_AUDIO_SOURCE@",
                            if m { "1" } else { "0" },
                        ],
                    )
                    .await?;
                }
            }
        }
        let after = self.audio_target(&v.target).await?;
        if v.percent.is_some_and(|p| {
            after["percent"]
                .as_u64()
                .is_none_or(|actual| actual.abs_diff(u64::from(p)) > 1)
        }) || v.muted.is_some_and(|m| after["muted"].as_bool() != Some(m))
        {
            return Err(format!(
                "audio command completed but desired state was not observed: {after}"
            ));
        }
        Ok(json!({"target":v.target,"before":before,"after":after,"verified":true}))
    }
    async fn monitor(&self, requested: Option<&str>) -> Result<String, String> {
        let monitors = self.hypr("monitors").await?;
        let list = monitors.as_array().ok_or("invalid monitor list")?;
        if let Some(name) = requested {
            if name.is_empty() || name.len() > 128 || name.chars().any(char::is_control) {
                return Err("invalid monitor name".into());
            }
            if !list.iter().any(|m| m["name"].as_str() == Some(name)) {
                return Err("monitor is not in the current monitor list".into());
            }
            Ok(name.to_owned())
        } else {
            list.iter()
                .find(|m| m["focused"] == true)
                .and_then(|m| m["name"].as_str())
                .map(str::to_owned)
                .ok_or_else(|| "no focused monitor was reported".into())
        }
    }
    async fn brightness(&self, monitor: Option<&str>) -> Result<Value, String> {
        let monitor = self.monitor(monitor).await?;
        let output = self
            .text(
                "omarchy-brightness-display",
                &["--no-osd", "--monitor", &monitor],
            )
            .await?;
        let percent = output
            .trim()
            .parse::<u8>()
            .ok()
            .filter(|p| *p <= 100)
            .ok_or("could not read monitor brightness percentage")?;
        Ok(json!({"monitor":monitor,"percent":percent}))
    }
    async fn set_brightness(&self, v: BrightnessSet) -> Result<Value, String> {
        if !(1..=100).contains(&v.percent) {
            return Err("brightness percent must be between 1 and 100".into());
        }
        let before = self.brightness(v.monitor.as_deref()).await?;
        let monitor = before["monitor"].as_str().ok_or("missing monitor")?;
        if before["percent"].as_u64() == Some(u64::from(v.percent)) {
            return Ok(json!({"changed":false,"after":before,"verified":true}));
        }
        self.success(
            "omarchy-brightness-display",
            &["--monitor", monitor, &format!("{}%", v.percent)],
        )
        .await?;
        let after = self.brightness(Some(monitor)).await?;
        if after["percent"]
            .as_u64()
            .is_none_or(|p| p.abs_diff(u64::from(v.percent)) > 1)
        {
            return Err(format!(
                "brightness command completed but requested percentage was not observed: {after}"
            ));
        }
        Ok(json!({"before":before,"after":after,"verified":true,"tolerance_percent":1}))
    }

    async fn toggle(&self, feature: &str) -> Result<bool, String> {
        match feature {
            "nightlight" => {
                let s = self.text("hyprctl", &["hyprsunset", "temperature"]).await?;
                let n = s
                    .split(|c: char| !c.is_ascii_digit())
                    .find(|s| !s.is_empty())
                    .and_then(|n| n.parse::<u32>().ok())
                    .filter(|n| (1000..=20000).contains(n))
                    .ok_or("nightlight temperature is unavailable")?;
                Ok(n < 6000)
            }
            "stay_awake" => {
                if !self.script_has("omarchy-toggle-idle", "stay-awake|")
                    && !self.script_has("omarchy-toggle-idle", "stay-awake)")
                {
                    return Err("installed idle helper has no explicit state interface".into());
                }
                let v: Value =
                    serde_json::from_str(&self.text("omarchy-toggle-idle", &["--status"]).await?)
                        .map_err(|e| e.to_string())?;
                v["enabled"]
                    .as_bool()
                    .ok_or_else(|| "invalid stay-awake status".into())
            }
            "bar_visible" => {
                if !self.script_has("omarchy-toggle-bar", "[toggle|on|off]") {
                    return Err("installed bar helper has no explicit state interface".into());
                }
                let r = self.run("omarchy-toggle-enabled", &["bar-off"]).await?;
                match r.code {
                    0 => Ok(false),
                    1 => Ok(true),
                    _ => Err(format!(
                        "could not read bar flag: {}",
                        bounded(&r.stderr, 500)
                    )),
                }
            }
            "animations" => {
                if !self.script_has("omarchy-toggle-animations", "[toggle|on|off]") {
                    return Err("installed animation helper has no explicit state interface".into());
                }
                let r = self
                    .run("omarchy-hyprland-toggle-enabled", &["no-animations"])
                    .await?;
                match r.code {
                    0 => Ok(false),
                    1 => Ok(true),
                    _ => Err(format!(
                        "could not read animation flag: {}",
                        bounded(&r.stderr, 500)
                    )),
                }
            }
            _ => Err("unknown feature".into()),
        }
    }
    async fn toggles(&self) -> Value {
        let mut values = Map::new();
        for feature in ["nightlight", "stay_awake", "bar_visible", "animations"] {
            values.insert(
                feature.into(),
                match self.toggle(feature).await {
                    Ok(v) => json!({"available":true,"enabled":v}),
                    Err(e) => json!({"available":false,"reason":bounded(&e,1000)}),
                },
            );
        }
        Value::Object(values)
    }
    async fn set_toggle(&self, v: ToggleSet) -> Result<Value, String> {
        let before = self.toggle(&v.feature).await?;
        if before == v.enabled {
            return Ok(
                json!({"feature":v.feature,"enabled":v.enabled,"changed":false,"verified":true}),
            );
        }
        match v.feature.as_str() {
            "nightlight" => {
                self.success("omarchy-toggle-nightlight", &[]).await?;
            }
            "stay_awake" => {
                self.success(
                    "omarchy-toggle-idle",
                    &[if v.enabled {
                        "stay-awake"
                    } else {
                        "allow-idle"
                    }],
                )
                .await?;
            }
            "bar_visible" => {
                self.success(
                    "omarchy-toggle-bar",
                    &[if v.enabled { "off" } else { "on" }],
                )
                .await?;
            }
            "animations" => {
                self.success(
                    "omarchy-toggle-animations",
                    &[if v.enabled { "on" } else { "off" }],
                )
                .await?;
            }
            _ => return Err("unknown feature".into()),
        }
        let after = self.toggle(&v.feature).await?;
        if after != v.enabled {
            return Err(format!(
                "{} command completed but requested state was not observed",
                v.feature
            ));
        }
        Ok(json!({"feature":v.feature,"enabled":after,"changed":true,"verified":true}))
    }

    async fn workspaces(&self) -> Result<Value, String> {
        let all = self.hypr("workspaces").await?;
        let list = all.as_array().ok_or("invalid workspaces response")?;
        let rows: Vec<_> = list.iter().take(100).map(workspace_view).collect();
        let active = self.hypr("activeworkspace").await?;
        Ok(json!({"workspaces":rows,"active":workspace_view(&active),"truncated":list.len()>100}))
    }
    async fn workspace_switch(&self, id: u32) -> Result<Value, String> {
        validate_workspace(id)?;
        let before = self.hypr("activeworkspace").await?;
        if before["id"].as_u64() == Some(u64::from(id)) {
            return Ok(json!({"changed":false,"active":workspace_view(&before),"verified":true}));
        }
        let target = id.to_string();
        self.dispatch(
            &format!("hl.dispatch(hl.dsp.focus({{workspace=\"{id}\"}}))"),
            &["dispatch", "workspace", &target],
        )
        .await?;
        let after=tokio::time::timeout(Duration::from_secs(1),async {
            loop {
                let active=self.hypr("activeworkspace").await?;
                if active["id"].as_u64()==Some(u64::from(id)) {return Ok::<_,String>(active);}
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }).await.map_err(|_|"workspace request was sent but the requested active workspace was not observed within 1 second")??;
        Ok(json!({"changed":true,"active":workspace_view(&after),"verified":true}))
    }
    async fn find_window(&self, id: &str) -> Result<Option<Value>, String> {
        let list = self.hypr("clients").await?;
        Ok(list
            .as_array()
            .ok_or("invalid Hyprland client list")?
            .iter()
            .find(|v| v["address"].as_str() == Some(id))
            .cloned())
    }
    async fn window_action(&self, v: WindowAction) -> Result<Value, String> {
        validate_window_action(&v)?;
        let before = self
            .find_window(&v.window_id)
            .await?
            .ok_or("window is not in the current Hyprland client list; refresh computer.windows")?;
        let selector = format!("address:{}", v.window_id);
        let desired = v.enabled.unwrap_or(false);
        if window_matches(&before, &v) {
            return Ok(json!({"changed":false,"window":window_view(&before),"verified":true}));
        }
        let action = if desired { "set" } else { "unset" };
        match v.action.as_str() {
            "floating"=>self.dispatch(&format!("hl.dispatch(hl.dsp.window.float({{window=\"{selector}\",action=\"{action}\"}}))"),&["dispatch",if desired {"setfloating"}else{"settiled"},&selector]).await?,
            "fullscreen"=>{
                if !self.has_lua().await {return Err("targeted fullscreen requires Hyprland's Lua dispatcher; no focus-changing fallback was attempted".into());}
                let output=self.text("hyprctl",&["eval",&format!("hl.dispatch(hl.dsp.window.fullscreen({{window=\"{selector}\",action=\"{action}\",mode=\"fullscreen\",layout_aware=false}}))")]).await?;
                if !output.trim().is_empty()&&output.trim()!="ok" {return Err(format!("fullscreen request rejected: {}",bounded(&output,500)));}
            },
            "move_to_workspace"=>{
                let id=v.workspace_id.unwrap();
                self.dispatch(&format!("hl.dispatch(hl.dsp.window.move({{window=\"{selector}\",workspace=\"{id}\",follow=false}}))"),&["dispatch","movetoworkspacesilent",&format!("{id},{selector}")]).await?;
            },
            "close"=>self.dispatch(&format!("hl.dispatch(hl.dsp.window.close({{window=\"{selector}\"}}))"),&["dispatch","closewindow",&selector]).await?,
            _=>unreachable!()
        }
        let after = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let w = self.find_window(&v.window_id).await?;
                if v.action == "close" && w.is_none() {
                    return Ok::<_, String>(None);
                }
                if w.as_ref().is_some_and(|w| window_matches(w, &v)) {
                    return Ok(w);
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await;
        match after {
            Ok(Ok(w))=>Ok(json!({"changed":true,"action":v.action,"window_id":v.window_id,"window":w.as_ref().map(window_view),"verified":true,"closed":w.is_none()})),
            Ok(Err(e))=>Err(e),
            Err(_) if v.action=="close"=>Ok(json!({"request_sent":true,"window_id":v.window_id,"closed":false,"verified":false,"reason":"The application may require a confirmation dialog; no force kill was attempted."})),
            Err(_)=>Err("window action was sent but desired state was not observed within 1 second; refresh computer.windows before retrying".into()),
        }
    }

    async fn apps(&self, v: Apps) -> Result<Value, String> {
        if v.query.chars().count() > 128 || !(1..=100).contains(&v.limit) {
            return Err("query must be at most 128 characters and limit between 1 and 100".into());
        }
        let (catalog, scan_truncated) = scan_apps(&self.application_dirs)?;
        let query = v.query.to_lowercase();
        let matching: Vec<_> = catalog
            .into_iter()
            .filter(|a| {
                a.id.to_lowercase().contains(&query) || a.name.to_lowercase().contains(&query)
            })
            .collect();
        let more = matching.len() > v.limit;
        let chosen: Vec<_> = matching.into_iter().take(v.limit).collect();
        Ok(
            json!({"apps":chosen.into_iter().map(|a|json!({"app_id":a.id,"name":a.name,"icon":a.icon})).collect::<Vec<_>>(),"truncated":scan_truncated||more,"query":v.query}),
        )
    }
    async fn app_launch(&self, id: &str) -> Result<Value, String> {
        if !valid_app_id(id) || !self.observed_apps.lock().await.contains(id) {
            return Err("app_id must come from the latest omarchy.apps result".into());
        }
        if !scan_apps(&self.application_dirs)?
            .0
            .iter()
            .any(|a| a.id == id)
        {
            return Err("application is no longer available; refresh omarchy.apps".into());
        }
        let launcher = self.executable("gtk-launch")?;
        let launcher = launcher.to_str().ok_or("launcher path is not UTF-8")?;
        let output = self
            .success(
                "systemd-run",
                &[
                    "--user",
                    "--quiet",
                    "--collect",
                    "--property=Type=exec",
                    "--property=StandardOutput=null",
                    "--property=StandardError=null",
                    "--",
                    launcher,
                    id,
                ],
            )
            .await?;
        Ok(
            json!({"app_id":id,"request_accepted":true,"window_observed":false,"warnings":bounded(output.stderr.trim(),1000)}),
        )
    }
    async fn menu(&self, v: MenuRequest) -> Result<Value, String> {
        if !self.script_has("omarchy-menu", "summon|close")
            && !self.script_has("omarchy-menu", "toggle / summon / close")
        {
            return Err(
                "installed Omarchy menu lacks the noninteractive show/close interface".into(),
            );
        }
        let route = v.route.as_deref().unwrap_or("root");
        if ![
            "root",
            "appearance",
            "appearance.theme",
            "appearance.font",
            "setup",
            "system",
        ]
        .contains(&route)
        {
            return Err("unsupported menu route".into());
        }
        let output = match v.action.as_str() {
            "show" => self.success("omarchy-menu", &["summon", route]).await?,
            "close" if v.route.is_none() => self.success("omarchy-menu", &["close"]).await?,
            "close" => return Err("route is only accepted for show".into()),
            _ => return Err("menu action must be show or close".into()),
        };
        Ok(
            json!({"action":v.action,"route":if v.action=="show" {Some(route)}else{None},"request_accepted":true,"visibility_observed":false,"warnings":bounded(output.stderr.trim(),1000)}),
        )
    }
    async fn battery(&self) -> Result<Value, String> {
        let text = self.text("omarchy-battery-status", &["--shell"]).await?;
        parse_battery(&text)
    }
    async fn network(&self) -> Result<Value, String> {
        let text = self.text("omarchy-network-status", &[]).await?;
        parse_network(&text)
    }
}

fn tool(
    name: &str,
    description: &str,
    properties: Value,
    required: &[&str],
    read_only: bool,
) -> Value {
    json!({"name":format!("ssmcp.omarchy.{name}"),"description":description,
        "inputSchema":{"type":"object","properties":properties,"required":required,"additionalProperties":false},
        "annotations":{"readOnlyHint":read_only,"destructiveHint":!read_only,"idempotentHint":read_only,"openWorldHint":false}})
}
fn parse<T: serde::de::DeserializeOwned>(value: Value) -> Result<T, String> {
    serde_json::from_value(value).map_err(|e| format!("invalid Omarchy arguments: {e}"))
}
fn bounded(value: &str, limit: usize) -> String {
    value.chars().take(limit).collect()
}
fn observed(value: Result<Value, String>) -> Value {
    match value {
        Ok(v) => v,
        Err(e) => json!({"available":false,"reason":bounded(&e,1000)}),
    }
}
fn executable_file(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        metadata.is_file()
    }
}
fn read_bounded(path: &Path, limit: usize) -> Result<String, String> {
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    if !file.metadata().map_err(|e| e.to_string())?.is_file() {
        return Err("not a regular file".into());
    }
    let mut bytes = Vec::new();
    file.take((limit + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() > limit {
        return Err("file exceeds read limit".into());
    }
    String::from_utf8(bytes).map_err(|e| e.to_string())
}
fn finish(mut value: Value) -> Result<Value, String> {
    loop {
        let full = result(value.clone());
        if serde_json::to_vec(&full).map_err(|e| e.to_string())?.len() <= RESULT_LIMIT {
            return Ok(full);
        }
        let object = value
            .as_object_mut()
            .ok_or("Omarchy result must be an object")?;
        let key = object
            .iter()
            .filter_map(|(k, v)| {
                v.as_array()
                    .filter(|a| !a.is_empty())
                    .map(|a| (k.clone(), a.len()))
            })
            .max_by_key(|(_, n)| *n)
            .map(|(k, _)| k)
            .ok_or("Omarchy result exceeds 24 KiB")?;
        object
            .get_mut(&key)
            .and_then(Value::as_array_mut)
            .unwrap()
            .pop();
        object.insert("truncated".into(), true.into());
    }
}
fn parse_names(text: &str) -> (Vec<String>, bool) {
    let mut names = BTreeSet::new();
    let mut truncated = false;
    for line in text.lines() {
        let name = line.trim();
        if name.is_empty() {
            continue;
        }
        if name.len() > 512 || name.chars().any(char::is_control) {
            truncated = true;
            continue;
        }
        if names.len() >= ITEM_LIMIT {
            truncated = true;
            break;
        }
        names.insert(name.to_owned());
    }
    (names.into_iter().collect(), truncated)
}
fn validate_label(name: &str) -> Result<(), String> {
    if name.is_empty()
        || name.chars().count() > 128
        || name.starts_with('-')
        || !name
            .chars()
            .all(|c| c.is_alphanumeric() || " _-.+()".contains(c))
    {
        return Err("name must be a listed label with letters, numbers, spaces, or _-.+()".into());
    }
    Ok(())
}
fn safe_slug(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s != "."
        && s != ".."
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
}
fn media_extension(path: &Path) -> bool {
    path.extension().and_then(|s| s.to_str()).is_some_and(|s| {
        [
            "jpg", "jpeg", "png", "gif", "bmp", "webp", "mp4", "m4v", "mov", "webm", "mkv", "avi",
        ]
        .contains(&s.to_ascii_lowercase().as_str())
    })
}
fn parse_percent(s: &str) -> Option<u64> {
    s.split_whitespace()
        .find_map(|word| word.strip_suffix('%').and_then(|n| n.parse().ok()))
}
fn parse_wpctl(s: &str) -> Result<(u64, bool), String> {
    let value = s
        .split_once("Volume:")
        .and_then(|(_, s)| s.split_whitespace().next())
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|v| v.is_finite() && *v >= 0.0 && *v <= 10.0)
        .ok_or("invalid wpctl volume")?;
    Ok(((value * 100.0).round() as u64, s.contains("[MUTED]")))
}
fn validate_workspace(id: u32) -> Result<(), String> {
    if (1..=9999).contains(&id) {
        Ok(())
    } else {
        Err("workspace_id must be between 1 and 9999".into())
    }
}
fn valid_window_id(id: &str) -> bool {
    id.strip_prefix("0x").is_some_and(|s| {
        !s.is_empty()
            && s.len() <= 16
            && s.bytes().all(|c| c.is_ascii_hexdigit())
            && u64::from_str_radix(s, 16).is_ok_and(|n| n != 0)
    })
}
fn validate_window_action(v: &WindowAction) -> Result<(), String> {
    if !valid_window_id(&v.window_id) {
        return Err("window_id must be a nonzero address from computer.windows".into());
    }
    match v.action.as_str() {
        "floating"|"fullscreen" if v.enabled.is_some()&&v.workspace_id.is_none()=>Ok(()),
        "move_to_workspace" if v.enabled.is_none()=>validate_workspace(v.workspace_id.ok_or("workspace_id is required")?),
        "close" if v.enabled.is_none()&&v.workspace_id.is_none()=>Ok(()),
        _=>Err("floating/fullscreen require enabled; move_to_workspace requires workspace_id; close accepts neither".into())
    }
}
fn workspace_view(v: &Value) -> Value {
    json!({"workspace_id":v["id"],"name":bounded(v["name"].as_str().unwrap_or(""),128),"monitor":bounded(v["monitor"].as_str().unwrap_or(""),128),"windows":v["windows"],"fullscreen":v["hasfullscreen"],"layout":v["tiledLayout"]})
}
fn window_view(v: &Value) -> Value {
    json!({"window_id":v["address"],"floating":v["floating"],"fullscreen":v["fullscreen"],"workspace_id":v["workspace"]["id"],"monitor":v["monitor"]})
}
fn window_matches(w: &Value, v: &WindowAction) -> bool {
    match v.action.as_str() {
        "floating" => w["floating"].as_bool() == v.enabled,
        "fullscreen" => w["fullscreen"]
            .as_u64()
            .is_some_and(|mode| mode <= 3 && (mode & 2 != 0) == v.enabled.unwrap_or(false)),
        "move_to_workspace" => w["workspace"]["id"].as_u64() == v.workspace_id.map(u64::from),
        _ => false,
    }
}
fn valid_app_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 256
        && !id.starts_with('-')
        && id.ends_with(".desktop")
        && id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
}
fn parse_desktop(text: &str, id: String) -> Option<App> {
    let mut group = false;
    let mut name = None;
    let mut icon = None;
    let mut visible = true;
    let mut app = false;
    let mut exec = false;
    let mut dbus = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            group = line == "[Desktop Entry]";
            continue;
        }
        if !group || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key {
            "Name" if !value.is_empty() => name = Some(bounded(value, 256)),
            "Icon" if !value.is_empty() => icon = Some(bounded(value, 256)),
            "Type" => app = value == "Application",
            "Exec" => exec = !value.trim().is_empty(),
            "DBusActivatable" => dbus = value == "true",
            "OnlyShowIn"
                if !value
                    .split(';')
                    .any(|d| matches!(d, "Hyprland" | "Omarchy")) =>
            {
                visible = false
            }
            "NotShowIn"
                if value
                    .split(';')
                    .any(|d| matches!(d, "Hyprland" | "Omarchy")) =>
            {
                visible = false
            }
            "Hidden" | "NoDisplay" if value == "true" => visible = false,
            _ => {}
        }
    }
    if !visible || !app || !(exec || dbus) {
        return None;
    }
    Some(App {
        id,
        name: name?,
        icon,
    })
}
fn scan_apps(dirs: &[PathBuf]) -> Result<(Vec<App>, bool), String> {
    let mut seen = BTreeSet::new();
    let mut apps = BTreeMap::new();
    let mut scanned = 0;
    let mut truncated = false;
    for root in dirs.iter().take(17) {
        let mut pending = vec![(root.clone(), 0)];
        while let Some((dir, depth)) = pending.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries {
                scanned += 1;
                if scanned > SCAN_LIMIT {
                    truncated = true;
                    break;
                }
                let Ok(entry) = entry else { continue };
                let path = entry.path();
                let Ok(kind) = entry.file_type() else {
                    continue;
                };
                if kind.is_dir() && depth < 3 {
                    pending.push((path, depth + 1));
                    continue;
                }
                if !(kind.is_file() || kind.is_symlink())
                    || path.extension().is_none_or(|s| s != "desktop")
                {
                    continue;
                }
                let Ok(relative) = path.strip_prefix(root) else {
                    continue;
                };
                let id = relative
                    .to_string_lossy()
                    .replace(std::path::MAIN_SEPARATOR, "-");
                if !valid_app_id(&id) || !seen.insert(id.clone()) {
                    continue;
                }
                let Ok(text) = read_bounded(&path, 64 * 1024) else {
                    continue;
                };
                if let Some(app) = parse_desktop(&text, id.clone()) {
                    apps.insert(id, app);
                }
            }
            if truncated {
                break;
            }
        }
        if truncated {
            break;
        }
    }
    Ok((apps.into_values().collect(), truncated))
}
fn parse_battery(text: &str) -> Result<Value, String> {
    if text.trim().is_empty() {
        return Ok(json!({"present":false}));
    }
    let fields: BTreeMap<_, _> = text.lines().filter_map(|s| s.split_once('\t')).collect();
    let percentage = fields
        .get("percentage")
        .and_then(|s| s.trim_end_matches('%').parse::<u8>().ok())
        .filter(|v| *v <= 100)
        .ok_or("invalid battery percentage")?;
    let value = |key| fields.get(key).map(|v| bounded(v, 256));
    Ok(
        json!({"present":true,"percentage":percentage,"state":value("state"),"rate":value("rate"),"capacity":value("size"),"time":value("time"),"cycles":value("cycles"),"charge_threshold":value("threshold")}),
    )
}
fn parse_network(text: &str) -> Result<Value, String> {
    let fields: Vec<_> = text.trim_end_matches('\n').split('\t').collect();
    let kind = fields.first().copied().unwrap_or("");
    if !["disconnected", "wifi", "ethernet"].contains(&kind) {
        return Err("invalid Omarchy network status".into());
    }
    let label = fields.get(1).map(|s| bounded(s, 256)).unwrap_or_default();
    Ok(
        json!({"kind":kind,"connected":kind!="disconnected","label":label,
        "signal_percent":if kind=="wifi" {fields.get(2).and_then(|s|s.parse::<u8>().ok()).filter(|n|*n<=100)}else{None},
        "frequency_mhz":if kind=="wifi" {fields.get(3).and_then(|s|s.parse::<u32>().ok())}else{None},
        "internet_reachability":"not_probed"}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn strict_shapes_and_native_tool_inventory() {
        let tools = Omarchy::tools();
        assert_eq!(tools.len(), 21);
        let names: BTreeSet<_> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names.len(), tools.len());
        assert!(names.iter().all(|n| n.starts_with("ssmcp.omarchy.")));
        assert!(
            tools
                .iter()
                .all(|t| t["inputSchema"]["additionalProperties"] == false)
        );
        assert!(parse::<Empty>(json!({"command":"anything"})).is_err());
        assert!(parse::<AudioSet>(json!({"target":"output","percent":-1})).is_err());
        assert!(parse::<MenuRequest>(json!({"action":"show","shell":"bad"})).is_err());
    }
    #[test]
    fn values_cannot_become_paths_options_or_dispatchers() {
        for s in [
            "",
            "../x",
            "a/b",
            ";touch /tmp/x",
            "$(id)",
            "--help",
            "Font&Name",
            "Font\nNext",
            "Font\"",
        ] {
            assert!(validate_label(s).is_err(), "{s}");
        }
        for s in [
            "Amber Dusk",
            "JetBrainsMono Nerd Font",
            "中文字体",
            "Noto_Sans-1.0",
        ] {
            assert!(validate_label(s).is_ok());
        }
        for s in ["0x0", "0x", "1", "0x123\"});evil()", "0xFFFFFFFFFFFFFFFFF"] {
            assert!(!valid_window_id(s));
        }
        assert!(valid_window_id("0xabcdef1234"));
        assert!(validate_workspace(0).is_err());
        assert!(validate_workspace(10000).is_err());
        assert!(
            validate_window_action(&WindowAction {
                window_id: "0x1".into(),
                action: "close".into(),
                enabled: Some(true),
                workspace_id: None
            })
            .is_err()
        );
        assert!(
            validate_window_action(&WindowAction {
                window_id: "0x1".into(),
                action: "move_to_workspace".into(),
                enabled: None,
                workspace_id: Some(2)
            })
            .is_ok()
        );
        for s in [
            "../evil.desktop",
            "--version.desktop",
            "x.desktop\n",
            "x/evil.desktop",
        ] {
            assert!(!valid_app_id(s));
        }
    }
    #[test]
    fn disabling_fullscreen_preserves_a_maximized_window() {
        let request = WindowAction {
            window_id: "0x1".into(),
            action: "fullscreen".into(),
            enabled: Some(false),
            workspace_id: None,
        };
        assert!(window_matches(&json!({"fullscreen":0}), &request));
        assert!(window_matches(&json!({"fullscreen":1}), &request));
        assert!(!window_matches(&json!({"fullscreen":2}), &request));
        assert!(!window_matches(&json!({"fullscreen":null}), &request));
    }
    #[test]
    fn structured_status_parsers_reject_ambiguous_data() {
        assert_eq!(parse_wpctl("Volume: 0.37 [MUTED]").unwrap(), (37, true));
        assert!(parse_wpctl("Volume: NaN").is_err());
        assert_eq!(
            parse_percent("front-left: 32768 / 50% / -18.06 dB"),
            Some(50)
        );
        let network = parse_network("wifi\t测试网络\t71\t5745\n").unwrap();
        assert_eq!(network["signal_percent"], 71);
        assert_eq!(network["internet_reachability"], "not_probed");
        assert!(parse_network("password\tsecret").is_err());
        assert_eq!(parse_battery("").unwrap()["present"], false);
        assert_eq!(
            parse_battery("percentage\t42%\nstate\tdischarging\nrate\t12W\n").unwrap()["percentage"],
            42
        );
        assert!(parse_battery("percentage\t150%").is_err());
    }
    #[test]
    fn desktop_hidden_override_and_non_application_entries_are_not_launchable() {
        let raw = "[Desktop Entry]\nType=Application\nName=Editor\nExec=editor %f\nIcon=editor\n";
        assert!(parse_desktop(raw, "editor.desktop".into()).is_some());
        assert!(
            parse_desktop(&(raw.to_owned() + "Hidden=true\n"), "editor.desktop".into()).is_none()
        );
        assert!(
            parse_desktop(
                &(raw.to_owned() + "NoDisplay=true\n"),
                "editor.desktop".into()
            )
            .is_none()
        );
        assert!(
            parse_desktop(
                "[Desktop Entry]\nType=Link\nName=X\nURL=file:///x\n",
                "x.desktop".into()
            )
            .is_none()
        );
        assert!(
            parse_desktop(
                "[Desktop Action New]\nName=Fake\nExec=bad\n",
                "x.desktop".into()
            )
            .is_none()
        );
    }
    #[test]
    fn output_budget_counts_complete_mcp_result_and_preserves_utf8() {
        let rows: Vec<_> = (0..200)
            .map(|i| json!({"app_id":format!("app{i}.desktop"),"name":"测试".repeat(120)}))
            .collect();
        let out = finish(json!({"apps":rows})).unwrap();
        assert!(serde_json::to_vec(&out).unwrap().len() <= RESULT_LIMIT);
        assert_eq!(out["structuredContent"]["truncated"], true);
        assert!(
            !out["structuredContent"]["apps"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        let text = out["content"][0]["text"].as_str().unwrap();
        let decoded: Value = serde_json::from_str(text).unwrap();
        assert_eq!(decoded, out["structuredContent"]);
    }

    #[cfg(target_os = "linux")]
    struct Fixture {
        directory: PathBuf,
        instance: Omarchy,
    }
    #[cfg(target_os = "linux")]
    impl Fixture {
        fn new() -> Self {
            let mut bytes = [0u8; 8];
            getrandom::fill(&mut bytes).unwrap();
            let directory = std::env::temp_dir().join(format!(
                "ssmcp-omarchy-{}-{:x}",
                std::process::id(),
                u64::from_ne_bytes(bytes)
            ));
            std::fs::create_dir_all(directory.join("bin")).unwrap();
            std::fs::create_dir_all(directory.join("home")).unwrap();
            std::fs::create_dir_all(directory.join("installation")).unwrap();
            let mut instance = Omarchy::new(directory.clone()).unwrap();
            instance.home = Some(directory.join("home"));
            instance.installation = Some(directory.join("installation"));
            instance.path = vec![directory.join("bin")];
            instance.application_dirs =
                vec![directory.join("home/apps"), directory.join("system/apps")];
            instance.hyprland = true;
            instance.timeout = Duration::from_secs(2);
            let this = Self {
                directory,
                instance,
            };
            this.helper("omarchy-version", "printf '4.5.0-test\\n'");
            this
        }
        fn helper(&self, name: &str, body: &str) {
            use std::os::unix::fs::PermissionsExt;
            let path = self.directory.join("bin").join(name);
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        fn app(&self, dir: usize, name: &str, text: &str) {
            let p = self.instance.application_dirs[dir].join(name);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
        }
    }
    #[cfg(target_os = "linux")]
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.directory);
        }
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn catalog_observation_hidden_overrides_and_launch_validation() {
        let f = Fixture::new();
        let app = "[Desktop Entry]\nType=Application\nName=Test App\nExec=fixture\n";
        f.app(0, "masked.desktop", &format!("{app}Hidden=true\n"));
        f.app(1, "masked.desktop", app);
        f.app(1, "visible.desktop", app);
        for i in 0..100 {
            f.app(
                1,
                &format!("large{i}.desktop"),
                &format!(
                    "[Desktop Entry]\nType=Application\nName={}\nExec=fixture\n",
                    "中文".repeat(100)
                ),
            );
        }
        let out = f
            .instance
            .perform("ssmcp.omarchy.apps", json!({"limit":100}))
            .await
            .unwrap();
        let list = out["structuredContent"]["apps"].as_array().unwrap();
        assert!(!list.iter().any(|a| a["app_id"] == "masked.desktop"));
        let observed = f.instance.observed_apps.lock().await.clone();
        assert_eq!(observed.len(), list.len());
        assert!(
            list.iter()
                .all(|a| observed.contains(a["app_id"].as_str().unwrap()))
        );
        assert!(
            f.instance
                .perform(
                    "ssmcp.omarchy.app_launch",
                    json!({"app_id":"unseen.desktop"})
                )
                .await
                .unwrap_err()
                .contains("latest")
        );
    }
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn off_stops_omarchy_before_dispatch_but_allows_observation() {
        let f = Fixture::new();
        f.helper(
            "hyprctl",
            r#"test "$1" = '-j' || { : > unexpected-mutation; exit 88; }
printf '{"id":2,"name":"2","monitor":"TEST","windows":0}\n'"#,
        );
        f.instance.control.set_local(false).unwrap();
        let status = f
            .instance
            .perform("ssmcp.omarchy.status", json!({}))
            .await
            .unwrap();
        assert_eq!(
            status["structuredContent"]["computer_use"]["enabled"],
            false
        );
        for suffix in [
            "workspace_switch",
            "background_set",
            "theme_set",
            "window_action",
            "menu",
            "app_launch",
        ] {
            assert_eq!(
                f.instance
                    .perform(
                        &format!("ssmcp.omarchy.{suffix}"),
                        json!({"workspace_id":2})
                    )
                    .await
                    .unwrap_err(),
                crate::computer::host_control::LOCKED
            );
        }
        assert!(!f.directory.join("unexpected-mutation").exists());
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn workspace_already_active_never_dispatches() {
        let f = Fixture::new();
        f.helper("hyprctl",r#"if [ "$1" = "-j" ] && [ "$2" = "activeworkspace" ]; then printf '{"id":2,"name":"2","monitor":"TEST","windows":0}\n'; else echo 'unexpected mutation' >&2; exit 87; fi"#);
        let out = f
            .instance
            .perform("ssmcp.omarchy.workspace_switch", json!({"workspace_id":2}))
            .await
            .unwrap();
        assert_eq!(out["structuredContent"]["changed"], false);
        assert_eq!(out["structuredContent"]["verified"], true);
    }
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn explicit_toggle_skips_equal_state_and_rejects_unobservable_helpers() {
        let f = Fixture::new();
        f.helper("omarchy-toggle-idle",r#"# stay-awake)
if [ "$1" = "--status" ]; then printf '{"enabled":true}\n'; else echo 'unexpected change' >&2; exit 88; fi"#);
        let out = f
            .instance
            .perform(
                "ssmcp.omarchy.toggle_set",
                json!({"feature":"stay_awake","enabled":true}),
            )
            .await
            .unwrap();
        assert_eq!(out["structuredContent"]["changed"], false);
        f.helper("omarchy-toggle-idle", "echo would-toggle");
        assert!(
            f.instance
                .perform("ssmcp.omarchy.toggles", json!({}))
                .await
                .unwrap()["structuredContent"]["stay_awake"]["available"]
                == false
        );
    }
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn background_selection_is_bound_to_enumerated_media() {
        let f = Fixture::new();
        let backgrounds = f
            .directory
            .join("home/.local/state/omarchy/current/theme/backgrounds");
        std::fs::create_dir_all(&backgrounds).unwrap();
        std::fs::write(backgrounds.join("one.png"), b"fixture").unwrap();
        let outside = f.directory.join("outside.png");
        std::fs::write(&outside, b"outside").unwrap();
        let list = f
            .instance
            .perform("ssmcp.omarchy.backgrounds", json!({}))
            .await
            .unwrap();
        assert_eq!(
            list["structuredContent"]["backgrounds"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        let err = f
            .instance
            .perform("ssmcp.omarchy.background_set", json!({"path":outside}))
            .await
            .unwrap_err();
        assert!(err.contains("not in"));
    }
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn native_menu_reports_acceptance_without_visibility_claim() {
        let f = Fixture::new();
        f.helper(
            "omarchy-menu",
            "# toggle / summon / close\n[ \"$1\" = summon ] && [ \"$2\" = appearance ]",
        );
        let out = f
            .instance
            .perform(
                "ssmcp.omarchy.menu",
                json!({"action":"show","route":"appearance"}),
            )
            .await
            .unwrap();
        assert_eq!(out["structuredContent"]["request_accepted"], true);
        assert_eq!(out["structuredContent"]["visibility_observed"], false);
    }
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn bounded_command_output_timeout_and_shutdown_are_real() {
        let mut f = Fixture::new();
        f.helper("large", "/usr/bin/head -c 131073 /dev/zero");
        assert!(
            f.instance
                .text("large", &[])
                .await
                .unwrap_err()
                .contains("128 KiB")
        );
        f.instance.timeout = Duration::from_millis(80);
        f.helper("hang", "/usr/bin/sleep 60");
        let started = std::time::Instant::now();
        assert!(
            f.instance
                .text("hang", &[])
                .await
                .unwrap_err()
                .contains("timed out")
        );
        assert!(started.elapsed() < Duration::from_secs(6));
        f.instance.shutdown().await;
        assert!(
            f.instance
                .perform("ssmcp.omarchy.status", json!({}))
                .await
                .unwrap_err()
                .contains("closed")
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[ignore = "requires a live Omarchy session; getters only, no input or setting changes"]
    async fn readonly_live_smoke() {
        let provider = Omarchy::new(std::env::current_dir().unwrap()).unwrap();
        let status = provider
            .perform("ssmcp.omarchy.status", json!({}))
            .await
            .unwrap();
        assert_eq!(
            status["structuredContent"]["available"], true,
            "Omarchy installation required"
        );
        let mut summary = Vec::new();
        for (name, args) in [
            ("status", json!({})),
            ("themes", json!({})),
            ("backgrounds", json!({})),
            ("fonts", json!({})),
            ("audio", json!({})),
            ("brightness", json!({})),
            ("toggles", json!({})),
            ("workspaces", json!({})),
            ("apps", json!({"limit":20})),
            ("battery", json!({})),
            ("network", json!({})),
        ] {
            let started = std::time::Instant::now();
            match provider
                .perform(&format!("ssmcp.omarchy.{name}"), args)
                .await
            {
                Ok(output) => {
                    assert!(serde_json::to_vec(&output).unwrap().len() <= RESULT_LIMIT);
                    summary.push(json!({"tool":name,"ok":true,"elapsed_ms":started.elapsed().as_millis(),
                        "keys":output["structuredContent"].as_object().unwrap().keys().collect::<Vec<_>>()}));
                }
                Err(error) if name == "brightness" => {
                    summary.push(json!({"tool":name,"available":false,"reason":error}))
                }
                Err(error) => panic!("{name}: {error}"),
            }
        }
        provider.shutdown().await;
        println!(
            "OMARCHY_READONLY_SMOKE {}",
            serde_json::to_string(&summary).unwrap()
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn audio_setter_requires_observed_postcondition() {
        let f = Fixture::new();
        f.helper("omarchy-audio-output-sink", "printf 'fixture_sink\\n'");
        f.helper(
            "pactl",
            r#"case "$1" in
get-sink-volume) printf 'Volume: front-left: 32768 / 50%% / -18 dB\n';;
get-sink-mute) printf 'Mute: no\n';;
set-sink-mute) exit 0;;
*) exit 89;;
esac"#,
        );
        let error = f
            .instance
            .perform(
                "ssmcp.omarchy.audio_set",
                json!({"target":"output","muted":true}),
            )
            .await
            .unwrap_err();
        assert!(error.contains("desired state was not observed"), "{error}");
    }
}
