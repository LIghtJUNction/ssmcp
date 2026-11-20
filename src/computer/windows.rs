//! Windows desktop control through User32, GDI and the system GDI+ JPEG codec.
//! Coordinates are physical pixels in the virtual desktop. DPI awareness is
//! scoped to synchronous calls on the current thread and is always restored.
use super::windows_identity::{encode as window_id, parse as parse_window_id};
use super::*;
use std::{ffi::c_void, mem::size_of, path::Path, ptr};
use windows_sys::{
    Win32::{
        Foundation::{GetLastError, HANDLE, HWND, LPARAM, POINT, RECT, SetLastError},
        Graphics::{Gdi::*, GdiPlus as gdip},
        System::{
            Com::StructuredStorage::{CreateStreamOnHGlobal, GetHGlobalFromStream},
            Memory::{GlobalLock, GlobalSize, GlobalUnlock},
            StationsAndDesktops::*,
            Threading::GetCurrentThreadId,
        },
        UI::{
            HiDpi::{
                DPI_AWARENESS_CONTEXT, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE,
                DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetThreadDpiAwarenessContext,
            },
            Input::KeyboardAndMouse::*,
            WindowsAndMessaging::*,
        },
    },
    core::GUID,
};

const COORDINATES: &str = "desktop_physical";
const MAX_CAPTURE_PIXELS: u64 = 4 * 1024 * 1024;
static RELEASE_FAILURE: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

fn release_failure() -> Option<String> {
    RELEASE_FAILURE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}
fn retain_release_failure(error: String) {
    let mut failure = RELEASE_FAILURE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if failure.is_none() {
        *failure = Some(error);
    }
}
const INPUT_LIMITS: &str = "Windows allows input only on the current unlocked interactive desktop and into applications at an equal or lower integrity level (UIPI). Run ssmcp in the user's desktop session; secure desktops are not controlled.";

pub(super) async fn perform(_root: &Path, name: &str, args: Value) -> Result<Value, String> {
    if name == "ssmcp.computer.status" {
        parse::<Empty>(args)?;
        return Ok(result(status()));
    }
    if let Some(error) = release_failure() {
        return Err(format!(
            "a previous native input release failed: {error}; restore the interactive desktop and restart the client before issuing more control operations"
        ));
    }
    interactive_desktop()?;
    match name {
        "ssmcp.computer.monitors" => {
            parse::<Empty>(args)?;
            Ok(result(json!({
                "monitors": monitors()?,
                "coordinate_space": COORDINATES,
                "scale_meaning": "one coordinate unit is one physical desktop pixel"
            })))
        }
        "ssmcp.computer.windows" => {
            parse::<Empty>(args)?;
            Ok(result(windows()?))
        }
        "ssmcp.computer.focus" => {
            let v: Focus = parse(args)?;
            focus(&v.window_id).await?;
            Ok(result(json!({"focused":v.window_id})))
        }
        "ssmcp.computer.screenshot" => screenshot(parse(args)?),
        "ssmcp.computer.move" => {
            let p: Point = parse(args)?;
            move_pointer(p)?;
            Ok(result(
                json!({"x":p.x,"y":p.y,"coordinate_space":COORDINATES}),
            ))
        }
        "ssmcp.computer.click" => {
            let v: Click = parse(args)?;
            if !(1..=3).contains(&v.count) {
                return Err("count must be between 1 and 3".into());
            }
            if let Some(p) = optional_point(v.x, v.y)? {
                move_pointer(p)?;
            }
            mouse_click(v.button, v.count)?;
            Ok(result(json!({"clicked":true,"count":v.count})))
        }
        "ssmcp.computer.drag" => {
            drag(parse(args)?).await?;
            Ok(result(json!({"dragged":true,"button_released":true})))
        }
        "ssmcp.computer.key" => {
            key(parse(args)?)?;
            Ok(result(json!({"pressed":true,"released":true})))
        }
        "ssmcp.computer.type" => {
            let v: TypeText = parse(args)?;
            if v.text.len() > 16384 || v.text.contains('\0') {
                return Err("text must be at most 16384 UTF-8 bytes without NUL".into());
            }
            type_text(&v.text).await?;
            Ok(result(
                json!({"typed":true,"characters":v.text.chars().count()}),
            ))
        }
        "ssmcp.computer.scroll" => {
            scroll(parse(args)?)?;
            Ok(result(json!({"scrolled":true})))
        }
        _ => Err(format!("unknown computer tool: {name}")),
    }
}

fn last_error(action: &str) -> String {
    let error = unsafe { GetLastError() };
    format!(
        "{action}: Windows error {error}: {}",
        std::io::Error::from_raw_os_error(error as i32)
    )
}

struct DpiScope(DPI_AWARENESS_CONTEXT);
impl DpiScope {
    fn new() -> Result<Self, String> {
        // No process-wide DPI policy is changed. The old thread policy is
        // restored before any await can move this future to another thread.
        let mut previous =
            unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        if previous.is_null() {
            previous =
                unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE) };
        }
        if previous.is_null() {
            Err(last_error(
                "could not enter a physical-coordinate DPI context",
            ))
        } else {
            Ok(Self(previous))
        }
    }
}
impl Drop for DpiScope {
    fn drop(&mut self) {
        unsafe { SetThreadDpiAwarenessContext(self.0) };
    }
}

struct InputDesktop(HDESK);
impl Drop for InputDesktop {
    fn drop(&mut self) {
        unsafe { CloseDesktop(self.0) };
    }
}

fn object_name(handle: HANDLE) -> Result<String, String> {
    if handle.is_null() {
        return Err(last_error("desktop/window-station handle is unavailable"));
    }
    let mut text = [0_u16; 512];
    let mut needed = 0;
    if unsafe {
        GetUserObjectInformationW(
            handle,
            UOI_NAME,
            text.as_mut_ptr().cast(),
            size_of::<[u16; 512]>() as u32,
            &mut needed,
        )
    } == 0
    {
        return Err(last_error(
            "could not inspect desktop/window-station identity",
        ));
    }
    let count = text.iter().position(|&u| u == 0).unwrap_or(text.len());
    Ok(String::from_utf16_lossy(&text[..count]))
}

pub(super) fn interactive_desktop() -> Result<String, String> {
    let station = object_name(unsafe { GetProcessWindowStation() })?;
    if !station.eq_ignore_ascii_case("WinSta0") {
        return Err(format!(
            "noninteractive window station {station}; {INPUT_LIMITS}"
        ));
    }
    let input = unsafe { OpenInputDesktop(0, 0, DESKTOP_READOBJECTS) };
    if input.is_null() {
        return Err(format!(
            "{}; desktop may be locked or secure. {INPUT_LIMITS}",
            last_error("OpenInputDesktop failed")
        ));
    }
    let owned = InputDesktop(input);
    let input_name = object_name(owned.0)?;
    let current_name = object_name(unsafe { GetThreadDesktop(GetCurrentThreadId()) })?;
    if input_name != current_name || input_name.eq_ignore_ascii_case("Winlogon") {
        return Err(format!(
            "current desktop {current_name} is not the active unlocked desktop; {INPUT_LIMITS}"
        ));
    }
    Ok(input_name)
}

fn status() -> Value {
    let desktop = interactive_desktop();
    let geometry = if desktop.is_ok() {
        monitors()
    } else {
        Err("interactive desktop is unavailable".into())
    };
    let pending_release = release_failure();
    let ready = desktop.is_ok() && geometry.is_ok() && pending_release.is_none();
    let reason = pending_release
        .as_ref()
        .or(desktop.as_ref().err())
        .or(geometry.as_ref().err())
        .map(String::as_str);
    let cap = |backend: &str| capability(ready, backend, reason);
    json!({
        "platform":"windows","native":true,"session":"win32",
        "desktop":desktop.as_ref().ok(),"available":ready,"reason":reason,
        "unresolved_input_release":pending_release,
        "coordinate_space":COORDINATES,
        "coordinate_notes":"Physical pixels, including negative origins for monitors left of or above the primary display. Monitor scale is 1 because coordinates already represent physical pixels.",
        "permissions":{"interactive_desktop":desktop.is_ok(),"uipi":INPUT_LIMITS,"elevation_automatically_requested":false},
        "helpers":{},
        "capabilities":{
            "monitors":cap("EnumDisplayMonitors"),
            "windows":cap("EnumWindows"),
            "focus":cap("SetForegroundWindow with observed foreground verification"),
            "screenshot":cap("GDI capture + built-in GDI+ JPEG"),
            "move":cap("SetCursorPos"),
            "click":cap("SendInput"),
            "drag":cap("SendInput"),
            "key":cap("SendInput"),
            "type":cap("SendInput KEYEVENTF_UNICODE"),
            "scroll":cap("SendInput")
        }
    })
}

#[derive(Default)]
struct DisplayEnumeration {
    values: Vec<Monitor>,
    error: Option<String>,
}

unsafe extern "system" fn monitor_callback(
    handle: HMONITOR,
    _dc: HDC,
    _rect: *mut RECT,
    data: LPARAM,
) -> i32 {
    // EnumDisplayMonitors invokes this callback synchronously while data lives.
    let output = unsafe { &mut *(data as *mut DisplayEnumeration) };
    if output.values.len() >= 64 {
        output.error = Some("more than 64 monitors are not supported".into());
        return 0;
    }
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
    if unsafe { GetMonitorInfoW(handle, (&mut info as *mut MONITORINFOEXW).cast()) } == 0 {
        output.error = Some(last_error("GetMonitorInfoW failed"));
        return 0;
    }
    let rect = info.monitorInfo.rcMonitor;
    let region = match region_from_rect(rect) {
        Ok(region) => region,
        Err(error) => {
            output.error = Some(error);
            return 0;
        }
    };
    let name_len = info
        .szDevice
        .iter()
        .position(|&u| u == 0)
        .unwrap_or(info.szDevice.len());
    output.values.push(Monitor {
        name: String::from_utf16_lossy(&info.szDevice[..name_len]),
        x: region.x,
        y: region.y,
        width: region.width,
        height: region.height,
        pixel_width: region.width,
        pixel_height: region.height,
        scale: 1.0,
        focused: info.monitorInfo.dwFlags & 1 != 0,
    });
    1
}

fn monitors() -> Result<Vec<Monitor>, String> {
    let _dpi = DpiScope::new()?;
    let mut output = DisplayEnumeration::default();
    let success = unsafe {
        EnumDisplayMonitors(
            ptr::null_mut(),
            ptr::null(),
            Some(monitor_callback),
            (&mut output as *mut DisplayEnumeration) as LPARAM,
        )
    };
    if let Some(error) = output.error {
        return Err(error);
    }
    if success == 0 {
        return Err(last_error("EnumDisplayMonitors failed"));
    }
    if output.values.is_empty() {
        return Err("no active desktop monitors".into());
    }
    Ok(output.values)
}

fn region_from_rect(rect: RECT) -> Result<Region, String> {
    Region {
        x: rect.left,
        y: rect.top,
        width: u32::try_from(i64::from(rect.right) - i64::from(rect.left))
            .map_err(|_| "invalid monitor/window width")?,
        height: u32::try_from(i64::from(rect.bottom) - i64::from(rect.top))
            .map_err(|_| "invalid monitor/window height")?,
    }
    .validate()
}

#[derive(Default)]
struct WindowEnumeration {
    values: Vec<Value>,
    truncated: bool,
}
unsafe extern "system" fn window_callback(hwnd: HWND, data: LPARAM) -> i32 {
    let output = unsafe { &mut *(data as *mut WindowEnumeration) };
    if unsafe { IsWindowVisible(hwnd) } == 0 {
        return 1;
    }
    if output.values.len() >= 100 {
        output.truncated = true;
        return 0;
    }
    let mut rect = RECT::default();
    if unsafe { GetWindowRect(hwnd, &mut rect) } == 0 {
        return 1;
    }
    let mut pid = 0;
    unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
    if pid == 0 {
        return 1;
    }
    let mut title = [0_u16; 513];
    let count =
        unsafe { GetWindowTextW(hwnd, title.as_mut_ptr(), title.len() as i32) }.max(0) as usize;
    let mut class = [0_u16; 129];
    let class_count =
        unsafe { GetClassNameW(hwnd, class.as_mut_ptr(), class.len() as i32) }.max(0) as usize;
    output.values.push(json!({
        "window_id":window_id(hwnd as usize,pid),
        "title":String::from_utf16_lossy(&title[..count.min(title.len())]),
        "app":String::from_utf16_lossy(&class[..class_count.min(class.len())]),
        "pid":pid,"x":rect.left,"y":rect.top,
        "width":i64::from(rect.right)-i64::from(rect.left),
        "height":i64::from(rect.bottom)-i64::from(rect.top),
        "focused":(unsafe {GetForegroundWindow()})==hwnd,
        "minimized":unsafe {IsIconic(hwnd)}!=0,
    }));
    1
}

fn windows() -> Result<Value, String> {
    let _dpi = DpiScope::new()?;
    let mut output = WindowEnumeration::default();
    unsafe { SetLastError(0) };
    let success = unsafe {
        EnumWindows(
            Some(window_callback),
            (&mut output as *mut WindowEnumeration) as LPARAM,
        )
    };
    if success == 0 && !output.truncated {
        return Err(last_error("EnumWindows failed"));
    }
    Ok(json!({"windows":output.values,"truncated":output.truncated,"coordinate_space":COORDINATES}))
}

fn validate_window(handle: usize, pid: u32) -> Result<HWND, String> {
    let hwnd = handle as HWND;
    let mut actual_pid = 0;
    if unsafe { IsWindow(hwnd) } == 0 || unsafe { IsWindowVisible(hwnd) } == 0 {
        return Err("window is no longer visible; refresh computer.windows".into());
    }
    unsafe { GetWindowThreadProcessId(hwnd, &mut actual_pid) };
    if actual_pid != pid {
        return Err("window identity is stale; refresh computer.windows".into());
    }
    Ok(hwnd)
}
async fn focus(id: &str) -> Result<(), String> {
    let (handle, pid) = parse_window_id(id)?;
    {
        let _dpi = DpiScope::new()?;
        let hwnd = validate_window(handle, pid)?;
        if unsafe { IsIconic(hwnd) } != 0 {
            unsafe { ShowWindowAsync(hwnd, SW_RESTORE) };
        }
        // Windows foreground policy is respected; no AttachThreadInput bypass.
        unsafe { SetForegroundWindow(hwnd) };
    }
    let until = tokio::time::Instant::now() + Duration::from_millis(500);
    loop {
        interactive_desktop()?;
        let matches = {
            let hwnd = validate_window(handle, pid)?;
            (unsafe { GetForegroundWindow() }) == hwnd
        };
        if matches {
            return Ok(());
        }
        if tokio::time::Instant::now() >= until {
            return Err("Windows did not make the requested window foreground within 500 ms; foreground-lock policy or another application may have prevented focus".into());
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn validate_point(p: Point) -> Result<(), String> {
    p.validate()?;
    if !monitors()?.iter().any(|m| {
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
fn move_pointer(p: Point) -> Result<(), String> {
    interactive_desktop()?;
    validate_point(p)?;
    let _dpi = DpiScope::new()?;
    if unsafe { SetCursorPos(p.x, p.y) } == 0 {
        return Err(format!(
            "{}; {INPUT_LIMITS}",
            last_error("SetCursorPos failed")
        ));
    }
    let mut observed = POINT::default();
    if unsafe { GetCursorPos(&mut observed) } == 0 || observed.x != p.x || observed.y != p.y {
        return Err("pointer did not reach the requested coordinates; it may be clipped or moved by another input source".into());
    }
    Ok(())
}

fn mouse_input(flags: u32, data: u32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: 0,
                dy: 0,
                mouseData: data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}
fn keyboard_input(vk: u16, up: bool) -> INPUT {
    let extended = matches!(
        vk,
        VK_RCONTROL
            | VK_RMENU
            | VK_INSERT
            | VK_DELETE
            | VK_HOME
            | VK_END
            | VK_PRIOR
            | VK_NEXT
            | VK_LEFT
            | VK_RIGHT
            | VK_UP
            | VK_DOWN
            | VK_DIVIDE
            | VK_NUMLOCK
            | VK_SNAPSHOT
            | VK_LWIN
            | VK_RWIN
            | VK_APPS
    );
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: if up { KEYEVENTF_KEYUP } else { 0 }
                    | if extended { KEYEVENTF_EXTENDEDKEY } else { 0 },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}
fn unicode_input(unit: u16, up: bool) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: 0,
                wScan: unit,
                dwFlags: KEYEVENTF_UNICODE | if up { KEYEVENTF_KEYUP } else { 0 },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}
struct SendFailure {
    inserted: usize,
    message: String,
}
fn inject(inputs: &[INPUT]) -> Result<(), SendFailure> {
    if inputs.is_empty() {
        return Ok(());
    }
    unsafe { SetLastError(0) };
    let sent = unsafe {
        SendInput(
            inputs.len() as u32,
            inputs.as_ptr(),
            size_of::<INPUT>() as i32,
        )
    };
    if sent as usize != inputs.len() {
        return Err(SendFailure {
            inserted: sent as usize,
            message: format!(
                "{}; inserted {sent}/{} events. UIPI may block input without a specific error code. {INPUT_LIMITS}",
                last_error("SendInput failed"),
                inputs.len()
            ),
        });
    }
    Ok(())
}
fn send_raw(inputs: &[INPUT]) -> Result<(), String> {
    inject(inputs).map_err(|failure| failure.message)
}
fn send(inputs: &[INPUT]) -> Result<(), String> {
    interactive_desktop()?;
    send_raw(inputs)
}
fn send_guarded(inputs: &[INPUT], release: &mut ReleaseGuard) -> Result<(), String> {
    // Zero accepted events cannot leave one of our keys/buttons held. Keep
    // denied/UIPI-zero calls usable without inventing pending cleanup state.
    release.armed = false;
    interactive_desktop()?;
    match inject(inputs) {
        Ok(()) => {
            release.armed = true;
            Ok(())
        }
        Err(failure) => {
            release.armed = failure.inserted > 0;
            Err(failure.message)
        }
    }
}
struct ReleaseGuard {
    up: Vec<INPUT>,
    armed: bool,
    send_up: fn(&[INPUT]) -> Result<(), String>,
}
impl ReleaseGuard {
    fn new(up: Vec<INPUT>) -> Self {
        Self {
            up,
            armed: true,
            send_up: send_raw,
        }
    }
    fn release(&mut self) -> Result<(), String> {
        (self.send_up)(&self.up)?;
        self.armed = false;
        Ok(())
    }
    fn disarm(&mut self) {
        self.armed = false;
    }
}
impl Drop for ReleaseGuard {
    fn drop(&mut self) {
        // Never await in release: cancellation and every early error take
        // this synchronous path before the common operation lock is freed.
        if self.armed
            && let Err(error) = (self.send_up)(&self.up)
        {
            retain_release_failure(error);
        }
    }
}

fn ensure_keys_up(keys: &[u16]) -> Result<(), String> {
    for &key in keys {
        if unsafe { GetAsyncKeyState(i32::from(key)) } < 0 {
            return Err(format!(
                "key/button 0x{key:02x} is already held by another input source; release it before requesting computer input"
            ));
        }
    }
    Ok(())
}
fn ensure_modifiers_up() -> Result<(), String> {
    ensure_keys_up(&[
        VK_LCONTROL,
        VK_RCONTROL,
        VK_LMENU,
        VK_RMENU,
        VK_LSHIFT,
        VK_RSHIFT,
        VK_LWIN,
        VK_RWIN,
    ])
}
fn mouse_flags(button: Button) -> (u32, u32, u16) {
    match button {
        Button::Left => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, VK_LBUTTON),
        Button::Middle => (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, VK_MBUTTON),
        Button::Right => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, VK_RBUTTON),
    }
}
fn mouse_click(button: Button, count: u8) -> Result<(), String> {
    let (down, up, key) = mouse_flags(button);
    ensure_keys_up(&[key])?;
    for _ in 0..count {
        let mut release = ReleaseGuard::new(vec![mouse_input(up, 0)]);
        send_guarded(&[mouse_input(down, 0), mouse_input(up, 0)], &mut release)?;
        release.disarm();
    }
    Ok(())
}
async fn drag(v: Drag) -> Result<(), String> {
    if !(2..=64).contains(&v.path.len()) || v.duration_ms > 5000 {
        return Err("drag requires 2–64 points and duration_ms no greater than 5000".into());
    }
    for p in &v.path {
        validate_point(*p)?;
    }
    let (down, up, key) = mouse_flags(v.button);
    ensure_keys_up(&[key])?;
    move_pointer(v.path[0])?;
    let mut release = ReleaseGuard::new(vec![mouse_input(up, 0)]);
    send_guarded(&[mouse_input(down, 0)], &mut release)?;
    let delay = Duration::from_millis(v.duration_ms / (v.path.len() - 1) as u64);
    for p in &v.path[1..] {
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        move_pointer(*p)?;
    }
    release.release()
}

fn named_key(name: &str) -> Option<u16> {
    Some(match name {
        "Return" | "Enter" => VK_RETURN,
        "Tab" => VK_TAB,
        "Escape" | "Esc" => VK_ESCAPE,
        "BackSpace" | "Backspace" => VK_BACK,
        "Delete" => VK_DELETE,
        "Insert" => VK_INSERT,
        "Home" => VK_HOME,
        "End" => VK_END,
        "Prior" | "Page_Up" | "PageUp" => VK_PRIOR,
        "Next" | "Page_Down" | "PageDown" => VK_NEXT,
        "Left" | "ArrowLeft" => VK_LEFT,
        "Right" | "ArrowRight" => VK_RIGHT,
        "Up" | "ArrowUp" => VK_UP,
        "Down" | "ArrowDown" => VK_DOWN,
        "space" | "Space" => VK_SPACE,
        "Print" | "PrintScreen" => VK_SNAPSHOT,
        "Pause" => VK_PAUSE,
        "Caps_Lock" | "CapsLock" => VK_CAPITAL,
        "Num_Lock" | "NumLock" => VK_NUMLOCK,
        "Scroll_Lock" | "ScrollLock" => VK_SCROLL,
        "Menu" => VK_APPS,
        "Control_L" | "Control" => VK_LCONTROL,
        "Control_R" => VK_RCONTROL,
        "Alt_L" | "Alt" => VK_LMENU,
        "Alt_R" => VK_RMENU,
        "Shift_L" | "Shift" => VK_LSHIFT,
        "Shift_R" => VK_RSHIFT,
        "Super_L" | "Super" | "Meta_L" => VK_LWIN,
        "Super_R" | "Meta_R" => VK_RWIN,
        _ => {
            if let Some(number) = name
                .strip_prefix('F')
                .and_then(|s| s.parse::<u16>().ok())
                .filter(|n| (1..=24).contains(n))
            {
                return Some(VK_F1 + number - 1);
            }
            return None;
        }
    })
}
fn key_spec(name: &str) -> Result<(u16, Vec<u16>), String> {
    if let Some(vk) = named_key(name) {
        return Ok((vk, Vec::new()));
    }
    let mut chars = name.chars();
    if let (Some(ch), None) = (chars.next(), chars.next()) {
        let mut units = [0_u16; 2];
        if ch.encode_utf16(&mut units).len() == 1 {
            let code = unsafe { VkKeyScanW(units[0]) };
            if code != -1 {
                let flags = (code as u16) >> 8;
                if flags & !7 == 0 {
                    let mut modifiers = Vec::new();
                    if flags & 1 != 0 {
                        modifiers.push(VK_LSHIFT);
                    }
                    if flags & 2 != 0 {
                        modifiers.push(VK_LCONTROL);
                    }
                    if flags & 4 != 0 {
                        modifiers.push(VK_LMENU);
                    }
                    return Ok(((code as u16) & 0xff, modifiers));
                }
            }
        }
    }
    Err("unsupported key name for the current Windows keyboard layout; use a named key or computer.type for literal Unicode text".into())
}
fn key(v: Key) -> Result<(), String> {
    if v.key.is_empty() || v.key.len() > 64 || v.modifiers.len() > 4 {
        return Err("key requires a 1–64 byte name and at most four modifiers".into());
    }
    let (key, implicit) = key_spec(&v.key)?;
    let mut modifiers = Vec::new();
    for modifier in &v.modifiers {
        let vk = match modifier.as_str() {
            "ctrl" => VK_LCONTROL,
            "alt" => VK_LMENU,
            "shift" => VK_LSHIFT,
            "super" => VK_LWIN,
            _ => return Err("modifiers must be ctrl, alt, shift or super".into()),
        };
        if !modifiers.contains(&vk) {
            modifiers.push(vk);
        }
    }
    for vk in implicit {
        if !modifiers.contains(&vk) {
            modifiers.push(vk);
        }
    }
    modifiers.retain(|vk| *vk != key);
    ensure_modifiers_up()?;
    ensure_keys_up(&[key])?;
    let mut pressed = modifiers.clone();
    pressed.push(key);
    let mut up: Vec<_> = pressed
        .iter()
        .rev()
        .map(|&vk| keyboard_input(vk, true))
        .collect();
    let mut events: Vec<_> = pressed
        .iter()
        .map(|&vk| keyboard_input(vk, false))
        .collect();
    events.extend(up.iter().copied());
    let mut release = ReleaseGuard::new(std::mem::take(&mut up));
    send_guarded(&events, &mut release)?;
    release.disarm();
    Ok(())
}
async fn type_text(text: &str) -> Result<(), String> {
    if text.is_empty() {
        return Ok(());
    }
    ensure_modifiers_up()?;
    let units: Vec<_> = text.encode_utf16().collect();
    for chunk in units.chunks(128) {
        ensure_modifiers_up()?;
        let mut events = Vec::with_capacity(chunk.len() * 2);
        for &unit in chunk {
            events.push(unicode_input(unit, false));
            events.push(unicode_input(unit, true));
        }
        let mut release = ReleaseGuard::new(
            chunk
                .iter()
                .rev()
                .map(|&unit| unicode_input(unit, true))
                .collect(),
        );
        send_guarded(&events, &mut release)?;
        release.disarm();
        // Cancellation can only land between completely released chunks.
        tokio::task::yield_now().await;
    }
    Ok(())
}
fn scroll(v: Scroll) -> Result<(), String> {
    if !(1..=100).contains(&v.steps) {
        return Err("steps must be between 1 and 100".into());
    }
    let (flags, direction) = match v.direction.as_str() {
        "up" => (MOUSEEVENTF_WHEEL, 1),
        "down" => (MOUSEEVENTF_WHEEL, -1),
        "right" => (MOUSEEVENTF_HWHEEL, 1),
        "left" => (MOUSEEVENTF_HWHEEL, -1),
        _ => return Err("direction must be up, down, left or right".into()),
    };
    if let Some(p) = optional_point(v.x, v.y)? {
        move_pointer(p)?;
    }
    send(&[mouse_input(
        flags,
        (direction * i32::from(v.steps) * 120) as u32,
    )])
}

struct ScreenDc(HDC);
impl ScreenDc {
    fn new() -> Result<Self, String> {
        let dc = unsafe { GetDC(ptr::null_mut()) };
        if dc.is_null() {
            Err(last_error("GetDC failed"))
        } else {
            Ok(Self(dc))
        }
    }
}
impl Drop for ScreenDc {
    fn drop(&mut self) {
        unsafe { ReleaseDC(ptr::null_mut(), self.0) };
    }
}
struct CaptureBitmap {
    dc: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
}
impl CaptureBitmap {
    fn new(source: HDC, width: i32, height: i32) -> Result<Self, String> {
        let dc = unsafe { CreateCompatibleDC(source) };
        if dc.is_null() {
            return Err(last_error("CreateCompatibleDC failed"));
        }
        let mut owned = Self {
            dc,
            bitmap: ptr::null_mut(),
            previous: ptr::null_mut(),
        };
        owned.bitmap = unsafe { CreateCompatibleBitmap(source, width, height) };
        if owned.bitmap.is_null() {
            return Err(last_error("CreateCompatibleBitmap failed"));
        }
        owned.previous = unsafe { SelectObject(dc, owned.bitmap) };
        if owned.previous.is_null() || owned.previous as isize == -1 {
            owned.previous = ptr::null_mut();
            return Err(last_error("SelectObject failed"));
        }
        Ok(owned)
    }

    fn bgra_pixels(&mut self, width: u32, height: u32) -> Result<Vec<u8>, String> {
        if width == 0 || height == 0 || u64::from(width) * u64::from(height) > MAX_CAPTURE_PIXELS {
            return Err("capture exceeds the native pixel budget".into());
        }
        // GetDIBits requires deselection. A bitmap previously selected into a
        // DC must not be passed to GDI+ FromHBITMAP.
        if !self.previous.is_null() {
            let selected = unsafe { SelectObject(self.dc, self.previous) };
            if selected.is_null() || selected as isize == -1 {
                return Err(last_error("could not deselect the capture bitmap"));
            }
            self.previous = ptr::null_mut();
        }
        let mut info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width as i32,
                biHeight: -(height as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut pixels = vec![0_u8; (u64::from(width) * u64::from(height) * 4) as usize];
        let copied = unsafe {
            GetDIBits(
                self.dc,
                self.bitmap,
                0,
                height,
                pixels.as_mut_ptr().cast(),
                &mut info,
                DIB_RGB_COLORS,
            )
        };
        if copied != height as i32 {
            return Err(last_error("GetDIBits did not copy the complete capture"));
        }
        for pixel in pixels.as_chunks_mut::<4>().0.iter_mut() {
            pixel[3] = 255;
        }
        Ok(pixels)
    }
}
impl Drop for CaptureBitmap {
    fn drop(&mut self) {
        unsafe {
            if !self.previous.is_null() {
                SelectObject(self.dc, self.previous);
            }
            if !self.bitmap.is_null() {
                DeleteObject(self.bitmap);
            }
            if !self.dc.is_null() {
                DeleteDC(self.dc);
            }
        }
    }
}
struct CursorBitmaps(ICONINFO);
impl Drop for CursorBitmaps {
    fn drop(&mut self) {
        unsafe {
            if !self.0.hbmColor.is_null() {
                DeleteObject(self.0.hbmColor);
            }
            if !self.0.hbmMask.is_null() {
                DeleteObject(self.0.hbmMask);
            }
        }
    }
}
fn draw_cursor(dc: HDC, region: Region, width: u32, height: u32) -> Result<bool, String> {
    let mut cursor = CURSORINFO {
        cbSize: size_of::<CURSORINFO>() as u32,
        ..Default::default()
    };
    if unsafe { GetCursorInfo(&mut cursor) } == 0 {
        return Err(last_error("GetCursorInfo failed"));
    }
    if cursor.flags & CURSOR_SHOWING == 0
        || !region.contains(Point {
            x: cursor.ptScreenPos.x,
            y: cursor.ptScreenPos.y,
        })
    {
        return Ok(false);
    }
    let mut info = ICONINFO::default();
    if unsafe { GetIconInfo(cursor.hCursor, &mut info) } == 0 {
        return Err(last_error("GetIconInfo failed"));
    }
    let info = CursorBitmaps(info);
    let mut bitmap = BITMAP::default();
    let source = if info.0.hbmColor.is_null() {
        info.0.hbmMask
    } else {
        info.0.hbmColor
    };
    if unsafe {
        GetObjectW(
            source,
            size_of::<BITMAP>() as i32,
            (&mut bitmap as *mut BITMAP).cast(),
        )
    } == 0
    {
        return Err(last_error("GetObjectW cursor bitmap failed"));
    }
    let sx = f64::from(width) / f64::from(region.width);
    let sy = f64::from(height) / f64::from(region.height);
    let x = ((f64::from(cursor.ptScreenPos.x) - f64::from(region.x) - f64::from(info.0.xHotspot))
        * sx)
        .round() as i32;
    let y = ((f64::from(cursor.ptScreenPos.y) - f64::from(region.y) - f64::from(info.0.yHotspot))
        * sy)
        .round() as i32;
    let icon_height = if info.0.hbmColor.is_null() {
        bitmap.bmHeight / 2
    } else {
        bitmap.bmHeight
    };
    let w = (f64::from(bitmap.bmWidth) * sx).round().max(1.0) as i32;
    let h = (f64::from(icon_height) * sy).round().max(1.0) as i32;
    if unsafe {
        DrawIconEx(
            dc,
            x,
            y,
            cursor.hCursor,
            w,
            h,
            0,
            ptr::null_mut(),
            DI_NORMAL,
        )
    } == 0
    {
        return Err(last_error("DrawIconEx failed"));
    }
    Ok(true)
}

struct GdiPlus(usize);
impl GdiPlus {
    fn new() -> Result<Self, String> {
        let input = gdip::GdiplusStartupInput {
            GdiplusVersion: 1,
            SuppressExternalCodecs: 1,
            ..Default::default()
        };
        let mut token = 0;
        let code = unsafe { gdip::GdiplusStartup(&mut token, &input, ptr::null_mut()) };
        if code != 0 {
            Err(format!("GDI+ initialization failed with status {code}"))
        } else {
            Ok(Self(token))
        }
    }
}
impl Drop for GdiPlus {
    fn drop(&mut self) {
        unsafe { gdip::GdiplusShutdown(self.0) };
    }
}
struct GdiImage<'a> {
    raw: *mut gdip::GpImage,
    _pixels: std::marker::PhantomData<&'a mut [u8]>,
}
impl<'a> GdiImage<'a> {
    fn from_bgra(pixels: &'a mut [u8], width: u32, height: u32) -> Result<Self, String> {
        if width == 0
            || height == 0
            || u64::from(width) * u64::from(height) > MAX_CAPTURE_PIXELS
            || pixels.len() as u64 != u64::from(width) * u64::from(height) * 4
        {
            return Err("invalid owned BGRA image dimensions".into());
        }
        let mut image = ptr::null_mut();
        let code = unsafe {
            gdip::GdipCreateBitmapFromScan0(
                width as i32,
                height as i32,
                (width * 4) as i32,
                0x26200a,
                pixels.as_mut_ptr(),
                &mut image,
            )
        };
        if code != 0 || image.is_null() {
            return Err(format!("GDI+ BGRA conversion failed with status {code}"));
        }
        Ok(Self {
            raw: image.cast(),
            _pixels: std::marker::PhantomData,
        })
    }
}
impl Drop for GdiImage<'_> {
    fn drop(&mut self) {
        unsafe { gdip::GdipDisposeImage(self.raw) };
    }
}

// The prefix of the public IStream ABI. These interfaces are supplied by
// Ole32; only Release and Seek are called through this checked, owned pointer.
#[repr(C)]
struct StreamVtable {
    query_interface: unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> i32,
    add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
    read: unsafe extern "system" fn(*mut c_void, *mut c_void, u32, *mut u32) -> i32,
    write: unsafe extern "system" fn(*mut c_void, *const c_void, u32, *mut u32) -> i32,
    seek: unsafe extern "system" fn(*mut c_void, i64, u32, *mut u64) -> i32,
}
struct MemoryStream(*mut c_void);
impl MemoryStream {
    fn new() -> Result<Self, String> {
        let mut stream = ptr::null_mut();
        let code = unsafe { CreateStreamOnHGlobal(ptr::null_mut(), 1, &mut stream) };
        if code < 0 || stream.is_null() {
            Err(format!(
                "CreateStreamOnHGlobal failed: 0x{:08x}",
                code as u32
            ))
        } else {
            Ok(Self(stream))
        }
    }
    fn vtable(&self) -> &StreamVtable {
        // CreateStreamOnHGlobal returned a live IStream, owned until Drop.
        unsafe { &**(self.0 as *const *const StreamVtable) }
    }
    fn bytes(&self) -> Result<Vec<u8>, String> {
        let mut length = 0;
        let code = unsafe { (self.vtable().seek)(self.0, 0, 2, &mut length) };
        if code < 0 {
            return Err(format!("IStream::Seek failed: 0x{:08x}", code as u32));
        }
        let length = usize::try_from(length).map_err(|_| "JPEG stream is too large")?;
        if length == 0 || length > CAPTURE_LIMIT {
            return Err("JPEG stream exceeds the bounded capture buffer".into());
        }
        let mut memory = ptr::null_mut();
        let code = unsafe { GetHGlobalFromStream(self.0, &mut memory) };
        if code < 0 || memory.is_null() {
            return Err(format!(
                "GetHGlobalFromStream failed: 0x{:08x}",
                code as u32
            ));
        }
        if unsafe { GlobalSize(memory) } < length {
            return Err("JPEG stream size exceeds its allocation".into());
        }
        let data = unsafe { GlobalLock(memory) };
        if data.is_null() {
            return Err(last_error("GlobalLock failed"));
        }
        // The stream is not used or mutated while its HGLOBAL is locked.
        let bytes = unsafe { std::slice::from_raw_parts(data.cast::<u8>(), length) }.to_vec();
        unsafe { GlobalUnlock(memory) };
        Ok(bytes)
    }
}
impl Drop for MemoryStream {
    fn drop(&mut self) {
        unsafe { (self.vtable().release)(self.0) };
    }
}

fn encode_image(image: *mut gdip::GpImage, quality: u8) -> Result<Vec<u8>, String> {
    let stream = MemoryStream::new()?;
    let jpeg = GUID::from_u128(0x557cf401_1a04_11d3_9a73_0000f81ef32e);
    let mut quality = u32::from(quality);
    let params = gdip::EncoderParameters {
        Count: 1,
        Parameter: [gdip::EncoderParameter {
            Guid: gdip::EncoderQuality,
            NumberOfValues: 1,
            Type: gdip::EncoderParameterValueTypeLong as u32,
            Value: (&mut quality as *mut u32).cast(),
        }],
    };
    let code = unsafe { gdip::GdipSaveImageToStream(image, stream.0, &jpeg, &params) };
    if code != 0 {
        return Err(format!(
            "system JPEG encoder failed with GDI+ status {code}"
        ));
    }
    stream.bytes()
}
fn capture_size(region: Region, max_width: u32, shrink: f64) -> (u32, u32) {
    let width_scale = (f64::from(max_width) / f64::from(region.width)).min(1.0);
    let pixel_scale = (MAX_CAPTURE_PIXELS as f64
        / (f64::from(region.width) * f64::from(region.height)))
    .sqrt()
    .min(1.0);
    let scale = width_scale.min(pixel_scale) * shrink;
    (
        (f64::from(region.width) * scale).floor().max(1.0) as u32,
        (f64::from(region.height) * scale).floor().max(1.0) as u32,
    )
}
fn check_region(region: Region, desktop: Region) -> Result<(), String> {
    region.validate()?;
    if region.x < desktop.x
        || region.y < desktop.y
        || i64::from(region.x) + i64::from(region.width)
            > i64::from(desktop.x) + i64::from(desktop.width)
        || i64::from(region.y) + i64::from(region.height)
            > i64::from(desktop.y) + i64::from(desktop.height)
    {
        return Err("capture region is outside the active physical desktop bounds".into());
    }
    Ok(())
}
fn screenshot(v: Screenshot) -> Result<Value, String> {
    if !(320..=2560).contains(&v.max_width) || !(30..=90).contains(&v.quality) {
        return Err("max_width must be 320–2560 and quality must be 30–90".into());
    }
    let desktop = bounds(&monitors()?)?;
    let region = v.region.unwrap_or(desktop);
    check_region(region, desktop)?;
    let _dpi = DpiScope::new()?;
    let _gdip = GdiPlus::new()?;
    let screen = ScreenDc::new()?;
    let mut shrink = 1.0;
    for _ in 0..6 {
        let (width, height) = capture_size(region, v.max_width, shrink);
        let mut bitmap = CaptureBitmap::new(screen.0, width as i32, height as i32)?;
        if unsafe { SetStretchBltMode(bitmap.dc, HALFTONE) } == 0 {
            return Err(last_error("SetStretchBltMode failed"));
        }
        unsafe { SetBrushOrgEx(bitmap.dc, 0, 0, ptr::null_mut()) };
        if unsafe {
            StretchBlt(
                bitmap.dc,
                0,
                0,
                width as i32,
                height as i32,
                screen.0,
                region.x,
                region.y,
                region.width as i32,
                region.height as i32,
                SRCCOPY | CAPTUREBLT,
            )
        } == 0
        {
            return Err(last_error("desktop capture failed"));
        }
        let cursor = if v.cursor {
            draw_cursor(bitmap.dc, region, width, height)?
        } else {
            false
        };
        let mut pixels = bitmap.bgra_pixels(width, height)?;
        let image = GdiImage::from_bgra(&mut pixels, width, height)?;
        let bytes = encode_image(image.raw, v.quality)?;
        let (actual_width, actual_height) = jpeg_dimensions(&bytes)?;
        if actual_width != width || actual_height != height {
            return Err("JPEG encoder returned unexpected dimensions".into());
        }
        if bytes.len() <= IMAGE_LIMIT {
            let metadata = json!({
                "mime_type":"image/jpeg","width":width,"height":height,"bytes":bytes.len(),
                "coordinate_space":COORDINATES,"region":region,"cursor":cursor,
                "image_to_desktop":{"origin_x":region.x,"origin_y":region.y,
                    "scale_x":f64::from(region.width)/f64::from(width),
                    "scale_y":f64::from(region.height)/f64::from(height)}
            });
            return Ok(
                json!({"content":[{"type":"text","text":metadata.to_string()},
                {"type":"image","mimeType":"image/jpeg","data":STANDARD.encode(bytes)}],
                "structuredContent":metadata,"isError":false}),
            );
        }
        shrink *= 0.7;
    }
    Err("screenshot exceeds 400 KiB after bounded downscaling; choose a smaller region".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_and_negative_desktop_bounds() {
        assert_eq!(
            parse_window_id(&window_id(0x1234, 42)).unwrap(),
            (0x1234, 42)
        );
        for invalid in [
            "0x1234",
            "hwnd:0:1",
            "hwnd:1:0",
            "hwnd:-1:2",
            "hwnd:1:2:3",
            "hwnd:1:",
        ] {
            assert!(parse_window_id(invalid).is_err(), "{invalid}");
        }
        let desktop = Region {
            x: -1920,
            y: -200,
            width: 4480,
            height: 1640,
        };
        assert!(
            check_region(
                Region {
                    x: -1900,
                    y: -100,
                    width: 1000,
                    height: 1000
                },
                desktop
            )
            .is_ok()
        );
        assert!(
            check_region(
                Region {
                    x: -1921,
                    y: 0,
                    width: 1,
                    height: 1
                },
                desktop
            )
            .is_err()
        );
        assert!(
            check_region(
                Region {
                    x: 2500,
                    y: 0,
                    width: 100,
                    height: 100
                },
                desktop
            )
            .is_err()
        );
    }

    #[test]
    fn capture_bounds_handle_extreme_aspect_ratios() {
        for (width, height) in [(100000, 100000), (100000, 1), (1, 100000), (3840, 2160)] {
            let region = Region {
                x: 0,
                y: 0,
                width,
                height,
            };
            let (w, h) = capture_size(region, 1600, 1.0);
            assert!(w > 0 && h > 0 && w <= 1600);
            assert!(u64::from(w) * u64::from(h) <= MAX_CAPTURE_PIXELS);
        }
    }

    #[test]
    fn named_keys_and_unicode_packets_cover_non_bmp_text() {
        assert_eq!(named_key("F24"), Some(VK_F24));
        assert_eq!(named_key("F25"), None);
        assert_eq!(named_key("ArrowLeft"), Some(VK_LEFT));
        let units: Vec<_> = "中🦀".encode_utf16().collect();
        assert_eq!(units.len(), 3);
        for unit in units {
            let down = unicode_input(unit, false);
            let up = unicode_input(unit, true);
            unsafe {
                assert_eq!(down.Anonymous.ki.wVk, 0);
                assert_eq!(down.Anonymous.ki.wScan, unit);
                assert_eq!(down.Anonymous.ki.dwFlags, KEYEVENTF_UNICODE);
                assert_eq!(up.Anonymous.ki.dwFlags, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP);
            }
        }
    }

    #[test]
    fn system_jpeg_encoder_preserves_in_memory_dimensions() {
        // No screen capture or input: exercise the actual GDI+ codec, COM stream
        // lifetime and JPEG parsing on an owned synthetic bitmap in Windows CI.
        let _gdip = GdiPlus::new().unwrap();
        let (width, height) = (64_i32, 48_i32);
        let mut pixels = vec![0_u8; (width * height * 4) as usize];
        for (i, pixel) in pixels.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            pixel.copy_from_slice(&[(i % 251) as u8, 100, 200, 255]);
        }
        let image = GdiImage::from_bgra(&mut pixels, width as u32, height as u32).unwrap();
        let jpeg = encode_image(image.raw, 70).unwrap();
        assert_eq!(
            jpeg_dimensions(&jpeg).unwrap(),
            (width as u32, height as u32)
        );
        assert!(jpeg.len() < IMAGE_LIMIT);
    }

    #[test]
    fn deselected_gdi_bitmap_converts_to_owned_bgra_without_screen_capture() {
        let dc = unsafe { CreateCompatibleDC(ptr::null_mut()) };
        assert!(!dc.is_null());
        let created = CaptureBitmap::new(dc, 64, 48);
        unsafe { DeleteDC(dc) };
        let mut bitmap = created.unwrap();
        assert_ne!(unsafe { PatBlt(bitmap.dc, 0, 0, 64, 48, WHITENESS) }, 0);
        let pixels = bitmap.bgra_pixels(64, 48).unwrap();
        assert!(bitmap.previous.is_null());
        assert_eq!(pixels.len(), 64 * 48 * 4);
        assert!(
            pixels
                .as_chunks::<4>()
                .0
                .iter()
                .all(|pixel| *pixel == [255; 4])
        );
    }

    #[tokio::test]
    async fn input_release_runs_when_cancelled_and_retries_a_failed_release() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static RELEASES: AtomicUsize = AtomicUsize::new(0);
        fn release(_: &[INPUT]) -> Result<(), String> {
            RELEASES.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        RELEASES.store(0, Ordering::SeqCst);
        let future = async {
            let _guard = ReleaseGuard {
                up: Vec::new(),
                armed: true,
                send_up: release,
            };
            std::future::pending::<()>().await;
        };
        assert!(
            tokio::time::timeout(Duration::from_millis(1), future)
                .await
                .is_err()
        );
        assert_eq!(RELEASES.load(Ordering::SeqCst), 1);
        fn fail_once(_: &[INPUT]) -> Result<(), String> {
            if RELEASES.fetch_add(1, Ordering::SeqCst) == 0 {
                Err("injected partial release".into())
            } else {
                Ok(())
            }
        }
        RELEASES.store(0, Ordering::SeqCst);
        {
            let mut guard = ReleaseGuard {
                up: Vec::new(),
                armed: true,
                send_up: fail_once,
            };
            assert!(guard.release().is_err());
        }
        assert_eq!(RELEASES.load(Ordering::SeqCst), 2);
        {
            let mut guard = ReleaseGuard {
                up: Vec::new(),
                armed: true,
                send_up: release,
            };
            guard.release().unwrap();
        }
        assert_eq!(RELEASES.load(Ordering::SeqCst), 3);
        fn denied(_: &[INPUT]) -> Result<(), String> {
            Err("injected denied release".into())
        }
        {
            let _guard = ReleaseGuard {
                up: Vec::new(),
                armed: true,
                send_up: denied,
            };
        }
        assert!(release_failure().unwrap().contains("denied"));
        let blocked = perform(Path::new("."), "ssmcp.computer.key", json!({"key":"a"})).await;
        assert!(
            blocked
                .unwrap_err()
                .contains("previous native input release failed")
        );
        // Only test teardown can clear this process-wide failure latch.
        *RELEASE_FAILURE.lock().unwrap() = None;
    }

    #[test]
    fn status_has_native_permission_diagnostics_without_input() {
        let s = status();
        assert_eq!(s["platform"], "windows");
        assert_eq!(s["native"], true);
        assert_eq!(s["coordinate_space"], COORDINATES);
        assert!(s["permissions"]["interactive_desktop"].is_boolean());
        assert!(s["permissions"]["uipi"].as_str().unwrap().contains("UIPI"));
        assert_eq!(s["capabilities"].as_object().unwrap().len(), 10);
    }
}
