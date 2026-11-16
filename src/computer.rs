//! Computer-use desktop primitives with native Linux, Windows and macOS adapters.
#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod windows;
use crate::backend::{process::Managed, result};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    process::Stdio,
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::Mutex,
};

const IMAGE_LIMIT: usize = 400 * 1024;
const CAPTURE_LIMIT: usize = 16 * 1024 * 1024;
const TEXT_LIMIT: usize = 256 * 1024;
const TIMEOUT: Duration = Duration::from_secs(10);
const RELEASE_HELPER_TIMEOUT: Duration = Duration::from_secs(3);
const RELEASE_WAIT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq)]
enum Session {
    Hyprland,
    Wayland,
    X11,
    Unavailable,
}

pub struct Computer {
    root: PathBuf,
    session: Session,
    serial: Mutex<()>,
    closed: AtomicBool,
    releases: Arc<ReleaseTasks>,
}

#[derive(Clone, Copy, Deserialize, Serialize, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
struct Point {
    x: i32,
    y: i32,
}
impl Point {
    fn validate(self) -> Result<Self, String> {
        if self.x.abs_diff(0) > 100000 || self.y.abs_diff(0) > 100000 {
            return Err("coordinates must be between -100000 and 100000".into());
        }
        Ok(self)
    }
}

#[derive(Clone, Copy, Deserialize, Serialize, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
struct Region {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
}
impl Region {
    fn validate(self) -> Result<Self, String> {
        Point {
            x: self.x,
            y: self.y,
        }
        .validate()?;
        if self.width == 0 || self.height == 0 || self.width > 100000 || self.height > 100000 {
            return Err("region width and height must be between 1 and 100000".into());
        }
        Ok(self)
    }
    fn contains(self, point: Point) -> bool {
        i64::from(point.x) >= i64::from(self.x)
            && i64::from(point.y) >= i64::from(self.y)
            && i64::from(point.x) < i64::from(self.x) + i64::from(self.width)
            && i64::from(point.y) < i64::from(self.y) + i64::from(self.height)
    }
}

#[derive(Clone, Deserialize, Serialize)]
struct Monitor {
    name: String,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    pixel_width: u32,
    pixel_height: u32,
    scale: f64,
    focused: bool,
}

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Button {
    #[default]
    Left,
    Middle,
    Right,
}
impl Button {
    fn x11(self) -> &'static str {
        match self {
            Self::Left => "1",
            Self::Middle => "2",
            Self::Right => "3",
        }
    }
    fn ydotool(self) -> u8 {
        match self {
            Self::Left => 0,
            Self::Middle => 2,
            Self::Right => 1,
        }
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Focus {
    window_id: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Screenshot {
    region: Option<Region>,
    #[serde(default = "default_width")]
    max_width: u32,
    #[serde(default = "default_quality")]
    quality: u8,
    #[serde(default)]
    cursor: bool,
}
fn default_width() -> u32 {
    1600
}
fn default_quality() -> u8 {
    70
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Click {
    x: Option<i32>,
    y: Option<i32>,
    #[serde(default)]
    button: Button,
    #[serde(default = "one")]
    count: u8,
}
fn one() -> u8 {
    1
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Drag {
    path: Vec<Point>,
    #[serde(default)]
    button: Button,
    #[serde(default = "default_duration")]
    duration_ms: u64,
}
fn default_duration() -> u64 {
    300
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Key {
    key: String,
    #[serde(default)]
    modifiers: Vec<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TypeText {
    text: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Scroll {
    direction: String,
    #[serde(default = "default_steps")]
    steps: u8,
    x: Option<i32>,
    y: Option<i32>,
}
fn default_steps() -> u8 {
    3
}

fn parse<T: serde::de::DeserializeOwned>(args: Value) -> Result<T, String> {
    serde_json::from_value(args).map_err(|e| format!("invalid computer arguments: {e}"))
}
fn optional_point(x: Option<i32>, y: Option<i32>) -> Result<Option<Point>, String> {
    match (x, y) {
        (None, None) => Ok(None),
        (Some(x), Some(y)) => Point { x, y }.validate().map(Some),
        _ => Err("x and y must be supplied together".into()),
    }
}
fn capability(ready: bool, backend: &str, detail: Option<&str>) -> Value {
    json!({"available":ready,"backend":backend,"reason":if ready {None}else{detail}})
}
impl Computer {
    pub fn new(root: PathBuf) -> Result<Self, String> {
        let root = root.canonicalize().map_err(|e| e.to_string())?;
        let session = if !cfg!(target_os = "linux") {
            Session::Unavailable
        } else if std::env::var_os("WAYLAND_DISPLAY").is_some() {
            if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some() {
                Session::Hyprland
            } else {
                Session::Wayland
            }
        } else if std::env::var_os("DISPLAY").is_some() {
            Session::X11
        } else {
            Session::Unavailable
        };
        Ok(Self {
            root,
            session,
            serial: Mutex::new(()),
            closed: AtomicBool::new(false),
            releases: Arc::new(ReleaseTasks::default()),
        })
    }

    pub fn tools() -> Vec<Value> {
        let coordinate = json!({"type":"integer","minimum":-100000,"maximum":100000});
        let point = json!({"type":"object","properties":{"x":coordinate,"y":coordinate},"required":["x","y"],"additionalProperties":false});
        let button = json!({"type":"string","enum":["left","middle","right"],"default":"left"});
        vec![
            tool(
                "status",
                "Inspect native desktop capabilities and missing system helpers; no screenshot or input is produced.",
                json!({}),
                &[],
                true,
            ),
            tool(
                "monitors",
                "List monitors in the adapter's global desktop coordinates, pixel dimensions and scales; read coordinate_space before input.",
                json!({}),
                &[],
                true,
            ),
            tool(
                "windows",
                "List up to 100 visible desktop windows with IDs suitable for focus.",
                json!({}),
                &[],
                true,
            ),
            tool(
                "focus",
                "Focus a window ID returned by computer.windows.",
                json!({"window_id":{"type":"string","maxLength":32}}),
                &["window_id"],
                false,
            ),
            tool(
                "screenshot",
                "Capture a bounded JPEG image. Use coordinate_space and image_to_desktop mapping: Linux/macOS use logical desktop coordinates; Windows uses physical virtual-desktop pixels. A region can capture one window.",
                json!({"region":{"type":"object","properties":{"x":coordinate,"y":coordinate,"width":{"type":"integer","minimum":1,"maximum":100000},"height":{"type":"integer","minimum":1,"maximum":100000}},"required":["x","y","width","height"],"additionalProperties":false},"max_width":{"type":"integer","minimum":320,"maximum":2560,"default":1600},"quality":{"type":"integer","minimum":30,"maximum":90,"default":70},"cursor":{"type":"boolean","default":false}}),
                &[],
                true,
            ),
            tool(
                "move",
                "Move the pointer to global desktop logical coordinates.",
                json!({"x":coordinate,"y":coordinate}),
                &["x", "y"],
                false,
            ),
            tool(
                "click",
                "Click at the current pointer or optional global logical coordinates. Supply x and y together.",
                json!({"x":coordinate,"y":coordinate,"button":button,"count":{"type":"integer","minimum":1,"maximum":3,"default":1}}),
                &[],
                false,
            ),
            tool(
                "drag",
                "Drag a button through 2–64 global logical points, releasing the button on completion or failure.",
                json!({"path":{"type":"array","items":point,"minItems":2,"maxItems":64},"button":button,"duration_ms":{"type":"integer","minimum":0,"maximum":5000,"default":300}}),
                &["path"],
                false,
            ),
            tool(
                "key",
                "Press and release a named key (for example Return, Tab, Escape or a) with optional ctrl, alt, shift or super modifiers; super maps to Windows/Command.",
                json!({"key":{"type":"string","minLength":1,"maxLength":64},"modifiers":{"type":"array","items":{"type":"string","enum":["ctrl","alt","shift","super"]},"maxItems":4,"default":[]}}),
                &["key"],
                false,
            ),
            tool(
                "type",
                "Type literal UTF-8 text into the focused window, without shell or clipboard interpretation.",
                json!({"text":{"type":"string","maxLength":16384}}),
                &["text"],
                false,
            ),
            tool(
                "scroll",
                "Scroll by wheel steps at the current pointer or optional global logical coordinates.",
                json!({"direction":{"type":"string","enum":["up","down","left","right"]},"steps":{"type":"integer","minimum":1,"maximum":100,"default":3},"x":coordinate,"y":coordinate}),
                &["direction"],
                false,
            ),
        ]
    }

    pub async fn perform(&self, name: &str, args: Value) -> Result<Value, String> {
        let _lock = self.serial.lock().await;
        if self.closed.load(Ordering::Acquire) {
            return Err("computer connection is closed".into());
        }
        self.releases.wait(RELEASE_WAIT_TIMEOUT).await?;
        let operation = async {
            match std::env::consts::OS {
                #[cfg(windows)]
                "windows" => windows::perform(&self.root, name, args).await,
                #[cfg(target_os = "macos")]
                "macos" => macos::perform(&self.root, name, args).await,
                _ => match name {
                    "ssmcp.computer.status" => {
                        parse::<Empty>(args)?;
                        Ok(result(self.status().await))
                    }
                    "ssmcp.computer.monitors" => {
                        parse::<Empty>(args)?;
                        Ok(result(
                            json!({"monitors":self.monitors().await?,"coordinate_space":"desktop_logical"}),
                        ))
                    }
                    "ssmcp.computer.windows" => {
                        parse::<Empty>(args)?;
                        Ok(result(self.windows().await?))
                    }
                    "ssmcp.computer.focus" => {
                        let v: Focus = parse(args)?;
                        self.focus(&v.window_id).await?;
                        Ok(result(json!({"focused":v.window_id})))
                    }
                    "ssmcp.computer.screenshot" => self.screenshot(parse(args)?).await,
                    "ssmcp.computer.move" => {
                        let p: Point = parse(args)?;
                        self.validate_point(p).await?;
                        self.move_pointer(p).await?;
                        Ok(result(
                            json!({"x":p.x,"y":p.y,"coordinate_space":"desktop_logical"}),
                        ))
                    }
                    "ssmcp.computer.click" => {
                        let v: Click = parse(args)?;
                        if !(1..=3).contains(&v.count) {
                            return Err("count must be between 1 and 3".into());
                        }
                        if let Some(p) = optional_point(v.x, v.y)? {
                            self.validate_point(p).await?;
                            self.move_pointer(p).await?;
                        }
                        self.click(v.button, v.count).await?;
                        Ok(result(json!({"clicked":true,"count":v.count})))
                    }
                    "ssmcp.computer.drag" => {
                        self.drag(parse(args)?).await?;
                        Ok(result(json!({"dragged":true,"button_released":true})))
                    }
                    "ssmcp.computer.key" => {
                        self.key(parse(args)?).await?;
                        Ok(result(json!({"pressed":true,"released":true})))
                    }
                    "ssmcp.computer.type" => {
                        let v: TypeText = parse(args)?;
                        if v.text.len() > 16384 || v.text.contains('\0') {
                            return Err("text must be at most 16384 UTF-8 bytes without NUL".into());
                        }
                        if !v.text.is_empty() {
                            self.type_text(&v.text).await?;
                        }
                        Ok(result(
                            json!({"typed":true,"characters":v.text.chars().count()}),
                        ))
                    }
                    "ssmcp.computer.scroll" => {
                        let v: Scroll = parse(args)?;
                        self.scroll(v).await?;
                        Ok(result(json!({"scrolled":true})))
                    }
                    _ => Err(format!("unknown computer tool: {name}")),
                },
            }
        };
        tokio::time::timeout(Duration::from_secs(30), operation)
            .await
            .map_err(|_| "computer operation timed out after 30 seconds".to_owned())?
    }

    pub async fn shutdown(&self) -> Result<(), String> {
        self.closed.store(true, Ordering::Release);
        tokio::time::timeout(RELEASE_WAIT_TIMEOUT, async {
            let _lock = self.serial.lock().await;
            self.releases.wait(RELEASE_WAIT_TIMEOUT).await
        })
        .await
        .map_err(|_| {
            "computer shutdown deadline exceeded; active input or release cleanup remains tracked"
                .to_owned()
        })?
    }

    async fn run(
        &self,
        program: &str,
        args: &[String],
        input: Option<&[u8]>,
        limit: usize,
    ) -> Result<Vec<u8>, String> {
        run_helper(&self.root, program, args, input, limit, TIMEOUT).await
    }
    async fn text(&self, program: &str, args: &[&str]) -> Result<String, String> {
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        String::from_utf8(self.run(program, &args, None, TEXT_LIMIT).await?)
            .map_err(|e| e.to_string())
    }
    async fn hypr_json(&self, command: &str) -> Result<Value, String> {
        serde_json::from_str(&self.text("hyprctl", &["-j", command]).await?)
            .map_err(|e| format!("invalid Hyprland response: {e}"))
    }

    async fn status(&self) -> Value {
        let names = [
            "hyprctl", "grim", "wtype", "ydotool", "xdotool", "magick", "xprop",
        ];
        let mut helpers = serde_json::Map::new();
        for name in names {
            helpers.insert(
                name.into(),
                match crate::program::executable(name) {
                    Ok(p) => json!({"installed":true,"path":p}),
                    Err(e) => json!({"installed":false,"reason":e}),
                },
            );
        }
        let session = match self.session {
            Session::Hyprland => "hyprland",
            Session::Wayland => "wayland",
            Session::X11 => "x11",
            Session::Unavailable => "unavailable",
        };
        if self.session == Session::Unavailable {
            return json!({"platform":std::env::consts::OS,"session":session,"available":false,
                "reason":"Native desktop control currently supports Linux Hyprland/Wayland and X11 sessions; a graphical session is required.",
                "helpers":helpers});
        }
        let installed = |name: &str| helpers[name]["installed"] == true;
        let geometry = self.monitors().await;
        let geometry_reason = geometry.as_ref().err().map(String::as_str);
        let x11 = self.session == Session::X11;
        let input_probe = if x11 {
            self.text("xdotool", &["getmouselocation", "--shell"]).await
        } else {
            self.text("ydotool", &["debug"]).await
        };
        let input_reason = input_probe.as_ref().err().map(String::as_str);
        let key_probe = if x11 {
            input_probe.clone()
        } else {
            self.text("wtype", &[""]).await
        };
        let key_reason = key_probe.as_ref().err().map(String::as_str);
        let image = if x11 {
            installed("magick")
        } else {
            installed("grim")
        };
        json!({"platform":std::env::consts::OS,"session":session,"coordinate_space":"desktop_logical",
        "helpers":helpers,"capabilities":{
            "monitors":capability(geometry.is_ok(),if x11 {"xdotool"}else{"hyprctl"},geometry_reason),
            "screenshot":capability(image && geometry.is_ok(),if x11 {"ImageMagick"}else{"grim"},Some("Install the screenshot helper and use a supported compositor with available monitor geometry.")),
            "windows":capability(if x11 {installed("xprop")&&installed("xdotool")}else{self.session==Session::Hyprland&&geometry.is_ok()},if x11 {"X11"}else{"Hyprland"},Some("Window enumeration and focus require Hyprland or X11.")),
            "move":capability(geometry.is_ok(),if x11 {"xdotool"}else{"hyprctl"},geometry_reason),
            "click":capability(input_probe.is_ok(),if x11 {"xdotool"}else{"ydotool"},input_reason),
            "drag":capability(input_probe.is_ok()&&geometry.is_ok(),if x11 {"xdotool"}else{"hyprctl + ydotool"},input_reason.or(geometry_reason)),
            "key":capability(key_probe.is_ok(),if x11 {"xdotool"}else{"wtype"},key_reason),
            "type":capability(key_probe.is_ok(),if x11 {"xdotool"}else{"wtype"},key_reason),
            "scroll":capability(input_probe.is_ok(),if x11 {"xdotool"}else{"ydotool"},input_reason)
        }})
    }

    async fn monitors(&self) -> Result<Vec<Monitor>, String> {
        match self.session {
            Session::Hyprland=>parse_hypr_monitors(self.hypr_json("monitors").await?),
            Session::X11=>{
                let t=self.text("xdotool",&["getdisplaygeometry"]).await?;
                let nums:Vec<u32>=t.split_whitespace().map(str::parse).collect::<Result<_,_>>().map_err(|_|"invalid X11 display geometry")?;
                if nums.len()!=2 || nums[0]==0 || nums[1]==0 {return Err("invalid X11 display geometry".into());}
                Ok(vec![Monitor{name:"X11 desktop".into(),x:0,y:0,width:nums[0],height:nums[1],pixel_width:nums[0],pixel_height:nums[1],scale:1.0,focused:true}])
            },
            Session::Wayland=>Err("This Wayland compositor has no native geometry adapter; Hyprland is supported. Xwayland coordinates are not used for the Wayland desktop.".into()),
            Session::Unavailable=>Err("No supported graphical desktop session is available".into()),
        }
    }
    async fn validate_point(&self, p: Point) -> Result<(), String> {
        p.validate()?;
        let monitors = self.monitors().await?;
        if !monitors.iter().any(|m| {
            Region {
                x: m.x,
                y: m.y,
                width: m.width,
                height: m.height,
            }
            .contains(p)
        }) {
            return Err("point is outside the active monitors; refresh computer.monitors".into());
        }
        Ok(())
    }
    async fn windows(&self) -> Result<Value, String> {
        match self.session {
            Session::Hyprland => {
                let raw = self.hypr_json("clients").await?;
                let windows = raw.as_array().ok_or("Hyprland returned no window array")?;
                let filtered: Vec<_> = windows
                    .iter()
                    .filter(|w| w["mapped"] == true && w["hidden"] != true)
                    .collect();
                let result:Vec<_>=filtered.iter().take(100).map(|w|json!({
                    "window_id":w["address"],"title":bounded(w["title"].as_str().unwrap_or(""),512),
                    "app":bounded(w["class"].as_str().unwrap_or(""),128),"pid":w["pid"],
                    "x":w["at"][0],"y":w["at"][1],"width":w["size"][0],"height":w["size"][1],
                    "workspace":w["workspace"]["name"],"xwayland":w["xwayland"]
                })).collect();
                Ok(
                    json!({"windows":result,"truncated":filtered.len()>100,"coordinate_space":"desktop_logical"}),
                )
            }
            Session::X11 => {
                let props = self.text("xprop", &["-root", "_NET_CLIENT_LIST"]).await?;
                let ids: Vec<_> = props
                    .split([' ', ',', '\n'])
                    .filter(|p| p.starts_with("0x"))
                    .take(101)
                    .collect();
                let mut windows = Vec::new();
                for id in ids.iter().take(100) {
                    if !valid_window_id(id) {
                        continue;
                    }
                    let title = self
                        .text("xdotool", &["getwindowname", id])
                        .await
                        .unwrap_or_default();
                    let geometry = self
                        .text("xdotool", &["getwindowgeometry", "--shell", id])
                        .await
                        .unwrap_or_default();
                    let mut item = json!({"window_id":id,"title":bounded(title.trim(),512)});
                    for line in geometry.lines() {
                        if let Some((k, v)) = line.split_once('=')
                            && matches!(k, "X" | "Y" | "WIDTH" | "HEIGHT")
                            && let Ok(v) = v.parse::<i64>()
                        {
                            item[k.to_ascii_lowercase()] = json!(v);
                        }
                    }
                    windows.push(item);
                }
                Ok(
                    json!({"windows":windows,"truncated":ids.len()>100,"coordinate_space":"desktop_logical"}),
                )
            }
            _ => Err("Window enumeration requires Hyprland or X11".into()),
        }
    }
    async fn focus(&self, id: &str) -> Result<(), String> {
        if !valid_window_id(id) {
            return Err("window_id must be a numeric ID returned by computer.windows".into());
        }
        let windows = self.windows().await?;
        if !windows["windows"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w["window_id"] == id)
        {
            return Err(
                "window is not in the current visible window list; refresh computer.windows".into(),
            );
        }
        match self.session {
            Session::Hyprland => {
                self.hypr_action(
                    format!("hl.dispatch(hl.dsp.focus({{window=\"address:{id}\"}}))"),
                    &["dispatch", "focuswindow", &format!("address:{id}")],
                )
                .await?;
                // A successful dispatcher call only acknowledges the request. Confirm
                // focus through read-only queries; never repeat the action on failure.
                let mut active = "unavailable".to_owned();
                tokio::time::timeout(Duration::from_millis(500), async {
                    loop {
                        let window = self.hypr_json("activewindow").await.map_err(|e| {
                            format!("Could not verify Hyprland focus for {id}: {e}")
                        })?;
                        let address = window["address"].as_str().unwrap_or("");
                        if address == id {
                            return Ok::<(), String>(());
                        }
                        active = if address.is_empty() {
                            "none".into()
                        } else {
                            bounded(address, 64)
                        };
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                })
                .await
                .map_err(|_| {
                    format!(
                        "Hyprland did not focus window {id} within 500 ms (active window: {active})"
                    )
                })??;
            }
            Session::X11 => {
                self.text("xdotool", &["windowactivate", "--sync", id])
                    .await?;
            }
            _ => return Err("Window focus requires Hyprland or X11".into()),
        }
        Ok(())
    }
    async fn hypr_action(&self, lua: String, legacy: &[&str]) -> Result<(), String> {
        // Presence of the Lua dispatcher is probed without issuing any input. A
        // failed action is never retried through another backend.
        let has_lua = self
            .text("hyprctl", &["repl", "return type(hl.dsp.cursor.move)"])
            .await
            .is_ok_and(|s| s.trim() == "function");
        let output = if has_lua {
            self.text("hyprctl", &["eval", &lua]).await?
        } else {
            self.text("hyprctl", legacy).await?
        };
        if output.trim() != "ok" && !output.trim().is_empty() {
            return Err(format!(
                "Hyprland action failed: {}",
                bounded(output.trim(), 1024)
            ));
        }
        Ok(())
    }
    async fn move_pointer(&self, p: Point) -> Result<(), String> {
        match self.session {
            Session::Hyprland => {
                self.hypr_action(
                    format!("hl.dispatch(hl.dsp.cursor.move({{x={},y={}}}))", p.x, p.y),
                    &["dispatch", "movecursor", &format!("{} {}", p.x, p.y)],
                )
                .await
            }
            Session::X11 => {
                self.text(
                    "xdotool",
                    &["mousemove", "--", &p.x.to_string(), &p.y.to_string()],
                )
                .await?;
                Ok(())
            }
            _ => Err("Exact pointer positioning requires Hyprland or X11".into()),
        }
    }
    async fn click(&self, button: Button, count: u8) -> Result<(), String> {
        let (program, args, release) = if self.session == Session::X11 {
            (
                "xdotool",
                vec![
                    "click".into(),
                    "--repeat".into(),
                    count.to_string(),
                    "--delay".into(),
                    "80".into(),
                    button.x11().into(),
                ],
                vec!["mouseup".into(), button.x11().into()],
            )
        } else if matches!(self.session, Session::Hyprland | Session::Wayland) {
            (
                "ydotool",
                vec![
                    "click".into(),
                    "--repeat".into(),
                    count.to_string(),
                    "--next-delay".into(),
                    "40".into(),
                    format!("0x{:X}", 0xC0 | button.ydotool()),
                ],
                vec!["click".into(), format!("0x{:X}", 0x80 | button.ydotool())],
            )
        } else {
            return Err("No desktop input session is available".into());
        };
        let mut guard = ReleaseGuard::new(program, release.clone(), &self.releases)?;
        let outcome = self.run(program, &args, None, TEXT_LIMIT).await;
        if outcome.is_ok() || self.run(program, &release, None, TEXT_LIMIT).await.is_ok() {
            guard.disarm();
        }
        outcome.map(|_| ())
    }
    async fn drag(&self, v: Drag) -> Result<(), String> {
        if !(2..=64).contains(&v.path.len()) || v.duration_ms > 5000 {
            return Err("drag requires 2–64 points and duration_ms at most 5000".into());
        }
        let monitors = self.monitors().await?;
        for p in &v.path {
            p.validate()?;
            if !monitors.iter().any(|m| {
                Region {
                    x: m.x,
                    y: m.y,
                    width: m.width,
                    height: m.height,
                }
                .contains(*p)
            }) {
                return Err("drag point is outside active monitors".into());
            }
        }
        let (program, down, up) = if self.session == Session::X11 {
            (
                "xdotool",
                vec!["mousedown".into(), v.button.x11().into()],
                vec!["mouseup".into(), v.button.x11().into()],
            )
        } else if self.session == Session::Hyprland {
            (
                "ydotool",
                vec!["click".into(), format!("0x{:X}", 0x40 | v.button.ydotool())],
                vec!["click".into(), format!("0x{:X}", 0x80 | v.button.ydotool())],
            )
        } else {
            return Err("Dragging requires Hyprland or X11".into());
        };
        self.move_pointer(v.path[0]).await?;
        let mut guard = ReleaseGuard::new(program, up.clone(), &self.releases)?;
        let outcome = async {
            self.run(program, &down, None, TEXT_LIMIT).await?;
            for p in &v.path[1..] {
                if v.duration_ms > 0 {
                    tokio::time::sleep(Duration::from_millis(
                        v.duration_ms / (v.path.len() - 1) as u64,
                    ))
                    .await;
                }
                self.move_pointer(*p).await?;
            }
            Ok::<(), String>(())
        }
        .await;
        let release = self.run(program, &up, None, TEXT_LIMIT).await;
        if release.is_ok() {
            guard.disarm();
        }
        outcome?;
        release.map(|_| ())
    }
    async fn key(&self, v: Key) -> Result<(), String> {
        if v.key.is_empty()
            || v.key.len() > 64
            || !v
                .key
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'_')
        {
            return Err("key must be an XKB key name using letters, digits or underscores".into());
        }
        if v.modifiers.len() > 4 {
            return Err("at most four modifiers are supported".into());
        }
        let mut seen = std::collections::BTreeSet::new();
        for m in &v.modifiers {
            if !matches!(m.as_str(), "ctrl" | "alt" | "shift" | "super") || !seen.insert(m) {
                return Err("modifiers must be unique ctrl, alt, shift or super values".into());
            }
        }
        match self.session {
            Session::X11 => {
                let mut chord = v.modifiers.clone();
                chord.push(v.key);
                let chord = chord.join("+");
                let mut guard = ReleaseGuard::new(
                    "xdotool",
                    vec!["keyup".into(), chord.clone()],
                    &self.releases,
                )?;
                let outcome = self
                    .text("xdotool", &["key", "--clearmodifiers", &chord])
                    .await;
                if outcome.is_ok() || self.text("xdotool", &["keyup", &chord]).await.is_ok() {
                    guard.disarm();
                }
                outcome.map(|_| ())
            }
            Session::Hyprland | Session::Wayland => {
                let mut args = Vec::new();
                for m in &v.modifiers {
                    args.extend([
                        "-M".into(),
                        if m == "super" {
                            "logo".into()
                        } else {
                            m.clone()
                        },
                    ]);
                }
                args.extend(["-k".into(), v.key]);
                for m in v.modifiers.iter().rev() {
                    args.extend([
                        "-m".into(),
                        if m == "super" {
                            "logo".into()
                        } else {
                            m.clone()
                        },
                    ]);
                }
                self.run("wtype", &args, None, TEXT_LIMIT).await.map(|_| ())
            }
            _ => Err("No desktop input session is available".into()),
        }
    }
    async fn type_text(&self, text: &str) -> Result<(), String> {
        match self.session {
            Session::X11 => self
                .run(
                    "xdotool",
                    &[
                        "type".into(),
                        "--clearmodifiers".into(),
                        "--delay".into(),
                        "0".into(),
                        "--file".into(),
                        "-".into(),
                    ],
                    Some(text.as_bytes()),
                    TEXT_LIMIT,
                )
                .await
                .map(|_| ()),
            Session::Hyprland | Session::Wayland => self
                .run("wtype", &["-".into()], Some(text.as_bytes()), TEXT_LIMIT)
                .await
                .map(|_| ()),
            _ => Err("No desktop input session is available".into()),
        }
    }
    async fn scroll(&self, v: Scroll) -> Result<(), String> {
        if !(1..=100).contains(&v.steps) {
            return Err("steps must be between 1 and 100".into());
        }
        let (x, y, button) = match v.direction.as_str() {
            "up" => (0, 1, "4"),
            "down" => (0, -1, "5"),
            "left" => (-1, 0, "6"),
            "right" => (1, 0, "7"),
            _ => return Err("direction must be up, down, left or right".into()),
        };
        if let Some(p) = optional_point(v.x, v.y)? {
            self.validate_point(p).await?;
            self.move_pointer(p).await?;
        }
        match self.session {
            Session::X11 => self
                .text(
                    "xdotool",
                    &[
                        "click",
                        "--repeat",
                        &v.steps.to_string(),
                        "--delay",
                        "10",
                        button,
                    ],
                )
                .await
                .map(|_| ()),
            Session::Hyprland | Session::Wayland => self
                .text(
                    "ydotool",
                    &[
                        "mousemove",
                        "--wheel",
                        "--",
                        &(x * i32::from(v.steps)).to_string(),
                        &(y * i32::from(v.steps)).to_string(),
                    ],
                )
                .await
                .map(|_| ()),
            _ => Err("No desktop input session is available".into()),
        }
    }

    async fn screenshot(&self, v: Screenshot) -> Result<Value, String> {
        if !(320..=2560).contains(&v.max_width) || !(30..=90).contains(&v.quality) {
            return Err("max_width must be 320–2560; quality must be 30–90".into());
        }
        let monitors = self.monitors().await?;
        let desktop = bounds(&monitors)?;
        let region = v.region.unwrap_or(desktop).validate()?;
        if i64::from(region.x) < i64::from(desktop.x)
            || i64::from(region.y) < i64::from(desktop.y)
            || i64::from(region.x) + i64::from(region.width)
                > i64::from(desktop.x) + i64::from(desktop.width)
            || i64::from(region.y) + i64::from(region.height)
                > i64::from(desktop.y) + i64::from(desktop.height)
        {
            return Err("capture region is outside desktop bounds".into());
        }
        let mut scale = (f64::from(v.max_width) / f64::from(region.width)).min(1.0);
        for _ in 0..5 {
            let bytes = match self.session {
                Session::Hyprland | Session::Wayland => {
                    let mut args = vec![
                        "-t".into(),
                        "jpeg".into(),
                        "-q".into(),
                        v.quality.to_string(),
                        "-s".into(),
                        format!("{scale:.6}"),
                        "-g".into(),
                        format!(
                            "{},{} {}x{}",
                            region.x, region.y, region.width, region.height
                        ),
                    ];
                    if v.cursor {
                        args.push("-c".into());
                    }
                    args.push("-".into());
                    self.run("grim", &args, None, CAPTURE_LIMIT).await?
                }
                Session::X11 => {
                    let geometry = format!(
                        "{}x{}{:+}{:+}",
                        region.width, region.height, region.x, region.y
                    );
                    let width = (f64::from(region.width) * scale).round().max(1.0) as u32;
                    let args = vec![
                        "import".into(),
                        "-window".into(),
                        "root".into(),
                        "-crop".into(),
                        geometry,
                        "+repage".into(),
                        "-resize".into(),
                        format!("{width}x"),
                        "-quality".into(),
                        v.quality.to_string(),
                        "jpeg:-".into(),
                    ];
                    self.run("magick", &args, None, CAPTURE_LIMIT).await?
                }
                _ => return Err("No desktop screenshot backend is available".into()),
            };
            let (width, height) = jpeg_dimensions(&bytes)?;
            if bytes.len() <= IMAGE_LIMIT {
                let metadata = json!({"mime_type":"image/jpeg","width":width,"height":height,"bytes":bytes.len(),
                    "coordinate_space":"desktop_logical","region":region,
                    "image_to_desktop":{"origin_x":region.x,"origin_y":region.y,"scale_x":f64::from(region.width)/f64::from(width),"scale_y":f64::from(region.height)/f64::from(height)},
                    "cursor":v.cursor && self.session!=Session::X11});
                return Ok(
                    json!({"content":[{"type":"text","text":metadata.to_string()},{"type":"image","mimeType":"image/jpeg","data":STANDARD.encode(bytes)}],"structuredContent":metadata,"isError":false}),
                );
            }
            scale *= 0.7;
        }
        Err("screenshot exceeds 400 KiB after bounded downscaling; select a smaller region".into())
    }
}

fn tool(
    name: &str,
    description: &str,
    properties: Value,
    required: &[&str],
    read_only: bool,
) -> Value {
    json!({"name":format!("ssmcp.computer.{name}"),"description":description,
        "inputSchema":{"type":"object","properties":properties,"required":required,"additionalProperties":false},
        "annotations":{"readOnlyHint":read_only,"destructiveHint":!read_only,"openWorldHint":false}})
}
fn bounded(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}
fn valid_window_id(id: &str) -> bool {
    if id.is_empty() || id.len() > 32 {
        return false;
    }
    if let Some(hex) = id.strip_prefix("0x") {
        !hex.is_empty() && hex.bytes().all(|b| b.is_ascii_hexdigit())
    } else {
        id.bytes().all(|b| b.is_ascii_digit())
    }
}
fn parse_hypr_monitors(raw: Value) -> Result<Vec<Monitor>, String> {
    let array = raw.as_array().ok_or("Hyprland returned no monitor array")?;
    let mut monitors = Vec::new();
    for m in array.iter().take(64).filter(|m| m["disabled"] != true) {
        let get = |key: &str| {
            m[key]
                .as_i64()
                .ok_or_else(|| format!("monitor has no valid {key}"))
        };
        let width = u32::try_from(get("width")?).map_err(|_| "invalid monitor width")?;
        let height = u32::try_from(get("height")?).map_err(|_| "invalid monitor height")?;
        let scale = m["scale"]
            .as_f64()
            .filter(|v| v.is_finite() && *v > 0.0)
            .ok_or("invalid monitor scale")?;
        let transformed = m["transform"].as_u64().unwrap_or(0) % 2 == 1;
        let (pw, ph) = if transformed {
            (height, width)
        } else {
            (width, height)
        };
        let logical_w = (f64::from(pw) / scale).round();
        let logical_h = (f64::from(ph) / scale).round();
        if !(1.0..=100000.0).contains(&logical_w) || !(1.0..=100000.0).contains(&logical_h) {
            return Err("unsupported monitor dimensions".into());
        }
        monitors.push(Monitor {
            name: m["name"].as_str().ok_or("monitor has no name")?.into(),
            x: i32::try_from(get("x")?).map_err(|_| "invalid monitor x")?,
            y: i32::try_from(get("y")?).map_err(|_| "invalid monitor y")?,
            width: logical_w as u32,
            height: logical_h as u32,
            pixel_width: pw,
            pixel_height: ph,
            scale,
            focused: m["focused"] == true,
        });
    }
    if monitors.is_empty() {
        return Err("no active monitor".into());
    }
    Ok(monitors)
}
fn bounds(monitors: &[Monitor]) -> Result<Region, String> {
    let minx = monitors
        .iter()
        .map(|m| i64::from(m.x))
        .min()
        .ok_or("no monitors")?;
    let miny = monitors.iter().map(|m| i64::from(m.y)).min().unwrap();
    let maxx = monitors
        .iter()
        .map(|m| i64::from(m.x) + i64::from(m.width))
        .max()
        .unwrap();
    let maxy = monitors
        .iter()
        .map(|m| i64::from(m.y) + i64::from(m.height))
        .max()
        .unwrap();
    Region {
        x: minx as i32,
        y: miny as i32,
        width: u32::try_from(maxx - minx).map_err(|_| "desktop too wide")?,
        height: u32::try_from(maxy - miny).map_err(|_| "desktop too high")?,
    }
    .validate()
}
fn jpeg_dimensions(bytes: &[u8]) -> Result<(u32, u32), String> {
    if !bytes.starts_with(&[0xff, 0xd8]) {
        return Err("capture helper returned no JPEG image".into());
    }
    let mut i = 2;
    while i + 4 <= bytes.len() {
        if bytes[i] != 0xff {
            return Err("invalid JPEG marker".into());
        }
        while i < bytes.len() && bytes[i] == 0xff {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let marker = bytes[i];
        i += 1;
        if marker == 0xda || marker == 0xd9 {
            break;
        }
        if marker == 0x01 || (0xd0..=0xd8).contains(&marker) {
            continue;
        }
        if i + 2 > bytes.len() {
            break;
        }
        let length = usize::from(u16::from_be_bytes([bytes[i], bytes[i + 1]]));
        if length < 2 || i + length > bytes.len() {
            return Err("truncated JPEG image".into());
        }
        if matches!(
            marker,
            0xc0 | 0xc1
                | 0xc2
                | 0xc3
                | 0xc5
                | 0xc6
                | 0xc7
                | 0xc9
                | 0xca
                | 0xcb
                | 0xcd
                | 0xce
                | 0xcf
        ) {
            if length < 8 {
                return Err("invalid JPEG dimensions".into());
            }
            let h = u32::from(u16::from_be_bytes([bytes[i + 3], bytes[i + 4]]));
            let w = u32::from(u16::from_be_bytes([bytes[i + 5], bytes[i + 6]]));
            if w == 0 || h == 0 {
                return Err("empty JPEG dimensions".into());
            }
            return Ok((w, h));
        }
        i += length;
    }
    Err("JPEG dimensions are missing".into())
}

async fn run_helper(
    root: &std::path::Path,
    program: &str,
    args: &[String],
    input: Option<&[u8]>,
    limit: usize,
    deadline: Duration,
) -> Result<Vec<u8>, String> {
    let executable = crate::program::executable(program)?;
    let mut command = tokio::process::Command::new(executable);
    command
        .args(args)
        .current_dir(root)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut process = Managed::spawn(&mut command).map_err(|e| format!("{program}: {e}"))?;
    let mut stdout = process
        .child
        .stdout
        .take()
        .ok_or("helper stdout unavailable")?
        .take((limit + 1) as u64);
    let mut stderr = process
        .child
        .stderr
        .take()
        .ok_or("helper stderr unavailable")?
        .take(8193);
    let mut stdin = process.child.stdin.take();
    let mut out = Vec::new();
    let mut err = Vec::new();
    let outcome = tokio::time::timeout(deadline, async {
        let writer = async {
            if let (Some(mut stdin), Some(input)) = (stdin.take(), input) {
                stdin.write_all(input).await.map_err(|e| e.to_string())?;
                stdin.shutdown().await.map_err(|e| e.to_string())?;
            }
            Ok::<(), String>(())
        };
        let (a, b, c, status) = tokio::join!(
            stdout.read_to_end(&mut out),
            stderr.read_to_end(&mut err),
            writer,
            process.wait()
        );
        a.map_err(|e| e.to_string())?;
        b.map_err(|e| e.to_string())?;
        c?;
        let status = status.map_err(|e| e.to_string())?;
        if out.len() > limit || err.len() > 8192 {
            return Err(format!("{program} exceeded its bounded output limit"));
        }
        if !status.success() {
            return Err(format!(
                "{program} failed ({status}): {}",
                bounded(String::from_utf8_lossy(&err).trim(), 1024)
            ));
        }
        Ok(out)
    })
    .await
    .map_err(|_| format!("{program} timed out after {} seconds", deadline.as_secs()))
    .and_then(|r| r);
    process.shutdown().await;
    outcome
}

#[derive(Default)]
struct ReleaseTasks {
    state: StdMutex<ReleaseState>,
}

#[derive(Default)]
struct ReleaseState {
    tasks: Vec<std::thread::JoinHandle<Result<(), String>>>,
    failure: Option<String>,
}

impl ReleaseTasks {
    fn start(self: &Arc<Self>, program: PathBuf, args: Vec<String>, deadline: Duration) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        // Keep the tracker alive even if a failed shutdown drops the Computer.
        // A task is complete only after its owned helper has actually been reaped.
        let keepalive = self.clone();
        match std::thread::Builder::new()
            .name("ssmcp-input-release".into())
            .spawn(move || {
                let result = release_helper(program, args, deadline);
                drop(keepalive);
                result
            }) {
            Ok(task) => state.tasks.push(task),
            Err(error) => {
                state.failure = Some(format!("could not start input release cleanup: {error}"));
            }
        }
    }

    async fn wait(&self, limit: Duration) -> Result<(), String> {
        let deadline = tokio::time::Instant::now() + limit;
        loop {
            let (pending, failure) = {
                let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                let mut index = 0;
                while index < state.tasks.len() {
                    if !state.tasks[index].is_finished() {
                        index += 1;
                        continue;
                    }
                    let outcome =
                        state.tasks.swap_remove(index).join().unwrap_or_else(|_| {
                            Err("input release cleanup thread panicked".into())
                        });
                    if let Err(error) = outcome {
                        state.failure.get_or_insert(error);
                    }
                }
                (state.tasks.len(), state.failure.clone())
            };
            if pending == 0 {
                return failure.map_or(Ok(()), Err);
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(format!(
                    "input release cleanup deadline exceeded; {pending} helper(s) remain tracked until reaped"
                ));
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

fn release_helper(program: PathBuf, args: Vec<String>, deadline: Duration) -> Result<(), String> {
    let mut child = std::process::Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("could not start input release helper: {error}"))?;
    let until = std::time::Instant::now() + deadline;
    let mut failure = None;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return failure.map_or_else(
                    || {
                        if status.success() {
                            Ok(())
                        } else {
                            Err(format!("input release helper failed ({status})"))
                        }
                    },
                    Err,
                );
            }
            Ok(None) => {
                if std::time::Instant::now() >= until {
                    failure.get_or_insert_with(|| "input release helper timed out".to_owned());
                    // A failed kill is not proof of exit. Keep tracking and
                    // reaping; the async waiter has its own bounded deadline.
                    let _ = child.kill();
                }
            }
            Err(error) => {
                failure
                    .get_or_insert_with(|| format!("could not reap input release helper: {error}"));
                let _ = child.kill();
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Cancellation can drop a future while a persistent virtual input device has a
/// held button. Register cleanup synchronously before the serial input lock is
/// released, so later input and shutdown can await the actual helper exit.
struct ReleaseGuard {
    executable: PathBuf,
    args: Vec<String>,
    armed: bool,
    releases: Arc<ReleaseTasks>,
    deadline: Duration,
}
impl ReleaseGuard {
    fn new(program: &str, args: Vec<String>, releases: &Arc<ReleaseTasks>) -> Result<Self, String> {
        Ok(Self {
            executable: crate::program::executable(program)?,
            args,
            armed: true,
            releases: releases.clone(),
            deadline: RELEASE_HELPER_TIMEOUT,
        })
    }
    fn disarm(&mut self) {
        self.armed = false;
    }
}
impl Drop for ReleaseGuard {
    fn drop(&mut self) {
        if self.armed {
            self.releases
                .start(self.executable.clone(), self.args.clone(), self.deadline);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_unknown_arguments_and_unpaired_coordinates() {
        assert!(parse::<Empty>(json!({"command":"rm"})).is_err());
        assert!(parse::<Point>(json!({"x":0,"y":1,"extra":true})).is_err());
        assert!(optional_point(Some(1), None).is_err());
        assert!(optional_point(Some(i32::MIN), Some(0)).is_err());
        assert!(parse::<Click>(json!({"button":"secondary"})).is_err());
        assert!(!valid_window_id("0xabc\"; exec evil"));
        assert!(!valid_window_id("0x"));
        assert!(valid_window_id("0x1ab"));
    }
    #[test]
    fn logical_coordinates_account_for_rotation_scale_and_negative_origin() {
        let monitors=parse_hypr_monitors(json!([
            {"name":"left","x":-900,"y":0,"width":1600,"height":900,"scale":1.0,"transform":1},
            {"name":"main","x":0,"y":0,"width":2560,"height":1600,"scale":1.25,"transform":0,"focused":true}
        ])).unwrap();
        assert_eq!((monitors[0].width, monitors[0].height), (900, 1600));
        assert_eq!((monitors[1].width, monitors[1].height), (2048, 1280));
        assert_eq!(
            bounds(&monitors).unwrap(),
            Region {
                x: -900,
                y: 0,
                width: 2948,
                height: 1600
            }
        );
        assert!(
            !Region {
                x: 0,
                y: 0,
                width: 100,
                height: 100
            }
            .contains(Point { x: 100, y: 50 })
        );
    }
    #[test]
    fn image_dimensions_parse_only_bounded_valid_markers() {
        let jpeg = [
            0xff, 0xd8, 0xff, 0xe0, 0, 4, 0, 0, 0xff, 0xc0, 0, 8, 8, 0, 100, 0, 200, 1,
        ];
        assert_eq!(jpeg_dimensions(&jpeg).unwrap(), (200, 100));
        assert!(jpeg_dimensions(&jpeg[..16]).is_err());
        assert!(jpeg_dimensions(b"not a screenshot").is_err());
        assert!(jpeg_dimensions(&[0xff, 0xd8, 0xff, 0xe0, 0, 0]).is_err());
    }
    #[tokio::test]
    async fn shutdown_revokes_input_before_dispatch() {
        let computer = Computer::new(std::env::temp_dir()).unwrap();
        computer.shutdown().await.unwrap();
        assert!(
            computer
                .perform("ssmcp.computer.move", json!({"x":0,"y":0}))
                .await
                .unwrap_err()
                .contains("closed")
        );
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn helpers_preserve_exit_errors_and_deadlines() {
        let args = vec![
            "-c".into(),
            "import sys;sys.stderr.write('native failure');sys.exit(7)".into(),
        ];
        let err = run_helper(
            &std::env::temp_dir(),
            "python3",
            &args,
            None,
            1024,
            Duration::from_secs(2),
        )
        .await
        .unwrap_err();
        assert!(err.contains("native failure"));
        let args = vec!["-c".into(), "import time;time.sleep(10)".into()];
        let err = run_helper(
            &std::env::temp_dir(),
            "python3",
            &args,
            None,
            1024,
            Duration::from_millis(30),
        )
        .await
        .unwrap_err();
        assert!(err.contains("timed out"));
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn helper_input_is_literal_and_output_limit_is_enforced() {
        let args = vec![
            "-c".into(),
            "import sys;sys.stdout.buffer.write(sys.stdin.buffer.read())".into(),
        ];
        let input = b"$(not-a-command) ' \n --file=/etc/passwd";
        assert_eq!(
            run_helper(
                &std::env::temp_dir(),
                "python3",
                &args,
                Some(input),
                1024,
                Duration::from_secs(2)
            )
            .await
            .unwrap(),
            input
        );
        let err = run_helper(
            &std::env::temp_dir(),
            "python3",
            &args,
            Some(&[b'a'; 100]),
            16,
            Duration::from_secs(2),
        )
        .await
        .unwrap_err();
        assert!(err.contains("bounded output"));
    }

    #[cfg(unix)]
    #[tokio::test]
    #[ignore = "subprocess fixture with a fake input helper"]
    async fn x11_failed_key_and_failed_release_fixture() {
        let Ok(root) = std::env::var("SSMCP_COMPUTER_RELEASE_FIXTURE") else {
            return;
        };
        let root = PathBuf::from(root);
        let computer = Computer {
            root: root.clone(),
            session: Session::X11,
            serial: Mutex::new(()),
            closed: AtomicBool::new(false),
            releases: Arc::new(ReleaseTasks::default()),
        };
        assert!(
            computer
                .key(Key {
                    key: "a".into(),
                    modifiers: vec!["ctrl".into()]
                })
                .await
                .is_err()
        );
        tokio::time::timeout(Duration::from_secs(3), async {
            while !root.join("released").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the fallback guard must retry release when explicit keyup fails");
        assert_eq!(
            std::fs::read_to_string(root.join("events")).unwrap(),
            "key\nkeyup\nkeyup\n"
        );
        computer.shutdown().await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failed_x11_keyup_keeps_fallback_release_armed() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!(
            "ssmcp-key-release-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let python = crate::program::executable("python3").unwrap();
        let helper = bin.join("xdotool");
        std::fs::write(
            &helper,
            format!(
                "#!{}\n{}",
                python.display(),
                r#"import os, pathlib, sys
root = pathlib.Path(os.environ["SSMCP_COMPUTER_RELEASE_FIXTURE"])
events = root / "events"
with events.open("a") as f:
    f.write(sys.argv[1] + "\n")
if sys.argv[1] == "key":
    sys.exit(7)
if events.read_text().count("keyup\n") == 1:
    sys.exit(8)
(root / "released").write_text("released")
"#
            ),
        )
        .unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut paths = vec![bin];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        let output = tokio::time::timeout(
            Duration::from_secs(8),
            tokio::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "computer::tests::x11_failed_key_and_failed_release_fixture",
                    "--ignored",
                    "--nocapture",
                ])
                .env("SSMCP_COMPUTER_RELEASE_FIXTURE", &root)
                .env("PATH", std::env::join_paths(paths).unwrap())
                .stdin(Stdio::null())
                .kill_on_drop(true)
                .output(),
        )
        .await
        .unwrap()
        .unwrap();
        let _ = std::fs::remove_dir_all(&root);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    #[cfg(unix)]
    struct ReleaseFixture(PathBuf);

    #[cfg(unix)]
    impl ReleaseFixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "ssmcp-release-drain-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir(&root).unwrap();
            Self(root)
        }

        fn guard(&self, computer: &Computer, script: &str) -> ReleaseGuard {
            ReleaseGuard {
                executable: crate::program::executable("python3").unwrap(),
                args: vec![
                    "-c".into(),
                    script.into(),
                    self.0.to_string_lossy().into_owned(),
                ],
                armed: true,
                releases: computer.releases.clone(),
                deadline: Duration::from_secs(10),
            }
        }

        async fn started(&self) {
            tokio::time::timeout(Duration::from_secs(5), async {
                while !self.0.join("started").exists() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("fake release helper must start");
        }

        fn release(&self) {
            std::fs::write(self.0.join("continue"), b"go").unwrap();
        }
    }

    #[cfg(unix)]
    impl Drop for ReleaseFixture {
        fn drop(&mut self) {
            let _ = std::fs::write(self.0.join("continue"), b"go");
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[cfg(unix)]
    const WAITING_RELEASE: &str = r#"import pathlib, sys, time
root = pathlib.Path(sys.argv[1])
(root / "started").write_text("started")
while root.exists() and not (root / "continue").exists():
    time.sleep(0.01)
if root.exists():
    (root / "released").write_text("released")
"#;

    #[cfg(unix)]
    #[tokio::test]
    async fn shutdown_awaits_release_after_cancelling_an_input_future() {
        let fixture = ReleaseFixture::new();
        let computer = Arc::new(Computer::new(fixture.0.clone()).unwrap());
        let guard = fixture.guard(&computer, WAITING_RELEASE);
        let entered = AtomicBool::new(false);
        let mut operation = Box::pin(async {
            let _serial = computer.serial.lock().await;
            let _guard = guard;
            entered.store(true, Ordering::Release);
            std::future::pending::<()>().await;
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut operation)
                .await
                .is_err()
        );
        assert!(entered.load(Ordering::Acquire));
        drop(operation);
        fixture.started().await;

        let mut shutdown = tokio::spawn({
            let computer = computer.clone();
            async move { computer.shutdown().await }
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut shutdown)
                .await
                .is_err()
        );
        assert!(!fixture.0.join("released").exists());
        fixture.release();
        tokio::time::timeout(Duration::from_secs(5), shutdown)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(fixture.0.join("released").exists());
        assert!(computer.releases.state.lock().unwrap().tasks.is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn release_wait_deadlines_preserve_tracking_and_order_later_input() {
        let fixture = ReleaseFixture::new();
        let computer = Arc::new(Computer::new(fixture.0.clone()).unwrap());
        drop(fixture.guard(&computer, WAITING_RELEASE));
        fixture.started().await;
        let error = computer
            .releases
            .wait(Duration::from_millis(20))
            .await
            .unwrap_err();
        assert!(error.contains("remain tracked"), "{error}");
        assert_eq!(computer.releases.state.lock().unwrap().tasks.len(), 1);
        assert!(
            tokio::time::timeout(
                Duration::from_millis(20),
                computer.releases.wait(RELEASE_WAIT_TIMEOUT)
            )
            .await
            .is_err()
        );
        assert_eq!(computer.releases.state.lock().unwrap().tasks.len(), 1);

        let mut next = tokio::spawn({
            let computer = computer.clone();
            async move {
                computer
                    .perform("ssmcp.computer.not_a_tool", json!({}))
                    .await
            }
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut next)
                .await
                .is_err()
        );
        fixture.release();
        let error = tokio::time::timeout(Duration::from_secs(5), next)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert!(error.contains("unknown computer tool"), "{error}");
        computer.shutdown().await.unwrap();
        assert!(computer.releases.state.lock().unwrap().tasks.is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    #[ignore = "subprocess fixture with a fake compositor helper"]
    async fn hyprland_focus_observation_fixture() {
        let Ok(root) = std::env::var("SSMCP_COMPUTER_FOCUS_FIXTURE") else {
            return;
        };
        let root = PathBuf::from(root);
        let case = std::env::var("SSMCP_COMPUTER_FOCUS_CASE").unwrap();
        assert_eq!(
            crate::program::executable("hyprctl").unwrap(),
            root.parent()
                .unwrap()
                .join("bin/hyprctl")
                .canonicalize()
                .unwrap()
        );
        let computer = Computer {
            root: root.clone(),
            session: Session::Hyprland,
            serial: Mutex::new(()),
            closed: AtomicBool::new(false),
            releases: Arc::new(ReleaseTasks::default()),
        };
        let start = std::time::Instant::now();
        let outcome = computer
            .perform("ssmcp.computer.focus", json!({"window_id":"0x123"}))
            .await;
        match case.as_str() {
            "immediate" | "delayed" => {
                assert_eq!(outcome.unwrap()["structuredContent"]["focused"], "0x123");
            }
            "mismatch" | "unresponsive" => {
                let error = outcome.unwrap_err();
                assert!(error.contains("did not focus window 0x123 within 500 ms"));
                if case == "mismatch" {
                    assert!(error.contains("active window: 0x999"));
                }
                assert!(
                    start.elapsed() < Duration::from_secs(2),
                    "focus verification must not inherit the helper's 10 second deadline"
                );
            }
            _ => panic!("unknown focus fixture case"),
        }
        let events: Vec<Vec<String>> = std::fs::read_to_string(root.join("events"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(
            events.iter().filter(|args| args[0] == "eval").count(),
            1,
            "focus must be dispatched exactly once"
        );
        let reads = events
            .iter()
            .filter(|args| args.as_slice() == ["-j", "activewindow"])
            .count();
        match case.as_str() {
            "immediate" | "unresponsive" => assert_eq!(reads, 1),
            "delayed" => assert_eq!(reads, 3),
            "mismatch" => assert!(reads >= 2),
            _ => unreachable!(),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shutdown_preserves_a_failed_release_after_reaping_the_helper() {
        let fixture = ReleaseFixture::new();
        let computer = Computer::new(fixture.0.clone()).unwrap();
        drop(fixture.guard(&computer, "import sys; sys.exit(7)"));
        let error = computer.shutdown().await.unwrap_err();
        assert!(error.contains("input release helper failed"), "{error}");
        assert!(error.contains('7'), "{error}");
        assert!(computer.releases.state.lock().unwrap().tasks.is_empty());
        assert_eq!(computer.shutdown().await.unwrap_err(), error);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn hyprland_focus_requires_observed_active_window() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!(
            "ssmcp-focus-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let python = crate::program::executable("python3").unwrap();
        let helper = bin.join("hyprctl");
        std::fs::write(
            &helper,
            format!(
                "#!{}\n{}",
                python.display(),
                r#"import json, os, pathlib, sys, time
root = pathlib.Path(os.environ["SSMCP_COMPUTER_FOCUS_FIXTURE"])
case = os.environ["SSMCP_COMPUTER_FOCUS_CASE"]
args = sys.argv[1:]
with (root / "events").open("a") as f:
    f.write(json.dumps(args) + "\n")
if args == ["-j", "clients"]:
    print(json.dumps([{"address": "0x123", "mapped": True, "hidden": False}]))
elif args == ["repl", "return type(hl.dsp.cursor.move)"]:
    print("function")
elif args == ["eval", 'hl.dispatch(hl.dsp.focus({window="address:0x123"}))']:
    print("ok")
elif args == ["-j", "activewindow"]:
    counter = root / "reads"
    reads = int(counter.read_text()) + 1 if counter.exists() else 1
    counter.write_text(str(reads))
    if case == "unresponsive":
        time.sleep(10)
    focused = case == "immediate" or (case == "delayed" and reads >= 3)
    print(json.dumps({"address": "0x123" if focused else "0x999"}))
else:
    sys.exit("unexpected fake compositor command")
"#
            ),
        )
        .unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut paths = vec![bin];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        let path = std::env::join_paths(paths).unwrap();
        let mut failures = Vec::new();
        for case in ["immediate", "delayed", "mismatch", "unresponsive"] {
            let case_root = root.join(case);
            std::fs::create_dir_all(&case_root).unwrap();
            let output = tokio::time::timeout(
                Duration::from_secs(5),
                tokio::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "computer::tests::hyprland_focus_observation_fixture",
                        "--ignored",
                        "--nocapture",
                    ])
                    .env("SSMCP_COMPUTER_FOCUS_FIXTURE", &case_root)
                    .env("SSMCP_COMPUTER_FOCUS_CASE", case)
                    .env("PATH", &path)
                    .stdin(Stdio::null())
                    .kill_on_drop(true)
                    .output(),
            )
            .await;
            match output {
                Ok(Ok(output)) if output.status.success() => {}
                Ok(Ok(output)) => failures.push(format!(
                    "{case}: {}\n{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                )),
                other => failures.push(format!("{case}: {other:?}")),
            }
        }
        let _ = std::fs::remove_dir_all(&root);
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }
}
