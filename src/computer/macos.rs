//! macOS desktop adapter. Quartz and Accessibility APIs never request permission.
//! Screen capture uses Apple's bounded system helpers rather than deprecated
//! CGDisplayCreateImage APIs. Coordinates are Quartz global desktop points.

use super::{
    Button, CAPTURE_LIMIT, Click, Drag, Empty, Focus, IMAGE_LIMIT, Key, Monitor, Point, Region,
    Screenshot, Scroll, TEXT_LIMIT, TIMEOUT, TypeText, bounded, bounds, capability,
    jpeg_dimensions, optional_point, parse, result, run_helper,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{
    ffi::{c_char, c_void},
    os::unix::fs::DirBuilderExt,
    path::{Path, PathBuf},
    ptr,
    time::{Duration, Instant},
};
use tokio::io::AsyncReadExt;

type Cf = *const c_void;
type Event = *mut c_void;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CgPoint {
    x: f64,
    y: f64,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CgSize {
    width: f64,
    height: f64,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CgRect {
    origin: CgPoint,
    size: CgSize,
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(value: Cf);
    fn CFRetain(value: Cf) -> Cf;
    fn CFGetTypeID(value: Cf) -> usize;
    fn CFArrayGetTypeID() -> usize;
    fn CFArrayGetCount(array: Cf) -> isize;
    fn CFArrayGetValueAtIndex(array: Cf, index: isize) -> Cf;
    fn CFDictionaryGetTypeID() -> usize;
    fn CFDictionaryGetValue(dictionary: Cf, key: Cf) -> Cf;
    fn CFNumberGetTypeID() -> usize;
    fn CFNumberGetValue(number: Cf, kind: isize, value: *mut c_void) -> u8;
    fn CFStringGetTypeID() -> usize;
    fn CFStringGetLength(value: Cf) -> isize;
    fn CFStringGetCString(value: Cf, buffer: *mut c_char, size: isize, encoding: u32) -> u8;
    fn CFStringCreateWithBytes(
        allocator: Cf,
        bytes: *const u8,
        length: isize,
        encoding: u32,
        external: u8,
    ) -> Cf;
    fn CFBooleanGetTypeID() -> usize;
    fn CFBooleanGetValue(value: Cf) -> u8;
    static kCFBooleanTrue: Cf;
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGGetActiveDisplayList(maximum: u32, displays: *mut u32, count: *mut u32) -> i32;
    fn CGMainDisplayID() -> u32;
    fn CGDisplayBounds(display: u32) -> CgRect;
    fn CGDisplayPixelsWide(display: u32) -> usize;
    fn CGDisplayPixelsHigh(display: u32) -> usize;
    fn CGWindowListCopyWindowInfo(options: u32, relative: u32) -> Cf;
    fn CGRectMakeWithDictionaryRepresentation(dictionary: Cf, rect: *mut CgRect) -> bool;
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGPreflightPostEventAccess() -> bool;
    fn CGEventCreate(source: Cf) -> Event;
    fn CGEventGetLocation(event: Cf) -> CgPoint;
    fn CGEventCreateMouseEvent(source: Cf, kind: u32, position: CgPoint, button: u32) -> Event;
    fn CGEventCreateKeyboardEvent(source: Cf, key: u16, down: bool) -> Event;
    fn CGEventCreateScrollWheelEvent(source: Cf, units: u32, wheel_count: u32, ...) -> Event;
    fn CGEventSetIntegerValueField(event: Event, field: u32, value: i64);
    fn CGEventSetFlags(event: Event, flags: u64);
    fn CGEventKeyboardSetUnicodeString(event: Event, length: usize, text: *const u16);
    fn CGEventPost(tap: u32, event: Cf);
    fn CGEventSourceKeyState(state: i32, key: u16) -> bool;
    fn CGEventSourceButtonState(state: i32, button: u32) -> bool;
    fn CGEventSourceFlagsState(state: i32) -> u64;
    static kCGWindowNumber: Cf;
    static kCGWindowOwnerPID: Cf;
    static kCGWindowOwnerName: Cf;
    static kCGWindowName: Cf;
    static kCGWindowLayer: Cf;
    static kCGWindowBounds: Cf;
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> u8;
    fn AXUIElementCreateApplication(pid: i32) -> Cf;
    fn AXUIElementSetMessagingTimeout(element: Cf, timeout: f32) -> i32;
    fn AXUIElementCopyAttributeValue(element: Cf, attribute: Cf, output: *mut Cf) -> i32;
    fn AXUIElementSetAttributeValue(element: Cf, attribute: Cf, value: Cf) -> i32;
    fn AXUIElementPerformAction(element: Cf, action: Cf) -> i32;
    fn AXValueGetTypeID() -> usize;
    fn AXValueGetValue(value: Cf, kind: u32, output: *mut c_void) -> bool;
}

const UTF8: u32 = 0x0800_0100;
const SHIFT: u64 = 1 << 17;
const CONTROL: u64 = 1 << 18;
const OPTION: u64 = 1 << 19;
const COMMAND: u64 = 1 << 20;
const MODIFIER_MASK: u64 = SHIFT | CONTROL | OPTION | COMMAND;
const MAX_CAPTURE_PIXELS: f64 = 64.0 * 1024.0 * 1024.0;
const MAX_CAPTURE_DIMENSION: f64 = 32768.0;
const SCREEN_CAPTURE: &str = "/usr/sbin/screencapture";
const IMAGE_PROCESSOR: &str = "/usr/bin/sips";
const ACCESSIBILITY_REASON: &str = "Accessibility permission is denied. Enable the ssmcp executable or its launching terminal in System Settings > Privacy & Security > Accessibility, then retry.";
const POST_EVENT_REASON: &str = "macOS event-posting permission is denied (CGPreflightPostEventAccess=false). Grant the ssmcp executable or launching terminal permission in System Settings > Privacy & Security, then retry.";
const SCREEN_REASON: &str = "Screen Recording permission is denied. Enable the ssmcp executable or its launching terminal in System Settings > Privacy & Security > Screen Recording (Screen & System Audio Recording on newer macOS), then restart the client if macOS requires it.";

struct Owned(Cf);
impl Owned {
    fn new(value: Cf, operation: &str) -> Result<Self, String> {
        if value.is_null() {
            Err(format!("{operation} returned no native object"))
        } else {
            Ok(Self(value))
        }
    }
    fn retained(value: Cf) -> Result<Self, String> {
        if value.is_null() {
            return Err("native object is missing".into());
        }
        // SAFETY: the caller supplies a borrowed CF object from a live container.
        Self::new(unsafe { CFRetain(value) }, "CFRetain")
    }
}
impl Drop for Owned {
    fn drop(&mut self) {
        // SAFETY: Owned is created only for a non-null Create/Copy/Retain result,
        // is not Clone, and releases its single ownership claim exactly once.
        unsafe { CFRelease(self.0) };
    }
}
fn cf_string(value: &str) -> Result<Owned, String> {
    // SAFETY: UTF-8 bytes and their exact length remain valid for this copying call.
    Owned::new(
        unsafe {
            CFStringCreateWithBytes(ptr::null(), value.as_ptr(), value.len() as isize, UTF8, 0)
        },
        "CFStringCreateWithBytes",
    )
}
fn cf_text(value: Cf) -> Option<String> {
    // SAFETY: type IDs are checked before using a typed CoreFoundation function.
    unsafe {
        if value.is_null() || CFGetTypeID(value) != CFStringGetTypeID() {
            return None;
        }
        let length = CFStringGetLength(value);
        if !(0..=16384).contains(&length) {
            return None;
        }
        let mut bytes = vec![0_u8; length as usize * 4 + 1];
        if CFStringGetCString(value, bytes.as_mut_ptr().cast(), bytes.len() as isize, UTF8) == 0 {
            return None;
        }
        let end = bytes.iter().position(|b| *b == 0)?;
        String::from_utf8(bytes[..end].to_vec()).ok()
    }
}
fn cf_i64(value: Cf) -> Option<i64> {
    // SAFETY: CFNumberGetValue writes exactly one signed 64-bit integer for type 4.
    unsafe {
        if value.is_null() || CFGetTypeID(value) != CFNumberGetTypeID() {
            return None;
        }
        let mut number = 0_i64;
        (CFNumberGetValue(value, 4, (&mut number as *mut i64).cast()) != 0).then_some(number)
    }
}
fn cf_bool(value: Cf) -> Option<bool> {
    // SAFETY: only a verified CFBoolean is read.
    unsafe {
        if value.is_null() || CFGetTypeID(value) != CFBooleanGetTypeID() {
            None
        } else {
            Some(CFBooleanGetValue(value) != 0)
        }
    }
}

fn region_from_rect(rect: CgRect) -> Result<Region, String> {
    let values = [
        rect.origin.x,
        rect.origin.y,
        rect.size.width,
        rect.size.height,
    ];
    if values.iter().any(|value| !value.is_finite()) {
        return Err("macOS returned non-finite desktop geometry".into());
    }
    // Outward rounding preserves the complete native window/display rectangle.
    let left = rect.origin.x.floor();
    let top = rect.origin.y.floor();
    let right = (rect.origin.x + rect.size.width).ceil();
    let bottom = (rect.origin.y + rect.size.height).ceil();
    if rect.size.width <= 0.0
        || rect.size.height <= 0.0
        || left < -100000.0
        || top < -100000.0
        || right > 100000.0
        || bottom > 100000.0
    {
        return Err("macOS returned unsupported desktop geometry".into());
    }
    Region {
        x: left as i32,
        y: top as i32,
        width: (right - left) as u32,
        height: (bottom - top) as u32,
    }
    .validate()
}

fn monitors() -> Result<Vec<Monitor>, String> {
    let mut ids = [0_u32; 64];
    let mut count = 0_u32;
    // SAFETY: ids has room for the advertised 64 display IDs.
    let error = unsafe { CGGetActiveDisplayList(64, ids.as_mut_ptr(), &mut count) };
    if error != 0 || count == 0 || count > 64 {
        return Err(format!(
            "No active macOS graphical display is available (CGGetActiveDisplayList={error})"
        ));
    }
    // SAFETY: display query functions do not retain or write caller memory.
    let main = unsafe { CGMainDisplayID() };
    ids[..count as usize]
        .iter()
        .map(|id| {
            let region = region_from_rect(unsafe { CGDisplayBounds(*id) })?;
            let pixel_width = u32::try_from(unsafe { CGDisplayPixelsWide(*id) })
                .map_err(|_| "macOS display pixel width overflow")?;
            let pixel_height = u32::try_from(unsafe { CGDisplayPixelsHigh(*id) })
                .map_err(|_| "macOS display pixel height overflow")?;
            if pixel_width == 0 || pixel_height == 0 {
                return Err("macOS returned an empty display".into());
            }
            Ok(Monitor {
                name: format!("display-{id}"),
                x: region.x,
                y: region.y,
                width: region.width,
                height: region.height,
                pixel_width,
                pixel_height,
                scale: f64::from(pixel_width) / f64::from(region.width),
                focused: *id == main,
            })
        })
        .collect()
}

#[derive(Clone)]
struct Window {
    id: u32,
    pid: i32,
    title: String,
    app: String,
    region: Region,
}
fn native_windows() -> Result<(Vec<Window>, bool), String> {
    // kCGWindowListOptionOnScreenOnly | kCGWindowListExcludeDesktopElements.
    // SAFETY: returned array follows the Copy ownership rule.
    let list = Owned::new(
        unsafe { CGWindowListCopyWindowInfo(17, 0) },
        "window server",
    )?;
    // SAFETY: CGWindowListCopyWindowInfo returns a CFArray of dictionaries.
    let count = unsafe { CFArrayGetCount(list.0) };
    if count < 0 {
        return Err("macOS returned an invalid window count".into());
    }
    let mut windows = Vec::new();
    let mut truncated = count > 4096;
    for index in 0..count.min(4096) {
        // SAFETY: index is within the retained array, and each value is validated.
        let dictionary = unsafe { CFArrayGetValueAtIndex(list.0, index) };
        unsafe {
            if dictionary.is_null() || CFGetTypeID(dictionary) != CFDictionaryGetTypeID() {
                continue;
            }
            if cf_i64(CFDictionaryGetValue(dictionary, kCGWindowLayer)) != Some(0) {
                continue;
            }
            let Some(id) = cf_i64(CFDictionaryGetValue(dictionary, kCGWindowNumber))
                .and_then(|v| u32::try_from(v).ok())
                .filter(|v| *v != 0)
            else {
                continue;
            };
            let Some(pid) = cf_i64(CFDictionaryGetValue(dictionary, kCGWindowOwnerPID))
                .and_then(|v| i32::try_from(v).ok())
                .filter(|v| *v > 0)
            else {
                continue;
            };
            let raw_bounds = CFDictionaryGetValue(dictionary, kCGWindowBounds);
            if raw_bounds.is_null() || CFGetTypeID(raw_bounds) != CFDictionaryGetTypeID() {
                continue;
            }
            let mut rect = CgRect::default();
            if !CGRectMakeWithDictionaryRepresentation(raw_bounds, &mut rect) {
                continue;
            }
            let Ok(region) = region_from_rect(rect) else {
                continue;
            };
            if windows.len() == 100 {
                truncated = true;
                break;
            }
            windows.push(Window {
                id,
                pid,
                title: cf_text(CFDictionaryGetValue(dictionary, kCGWindowName)).unwrap_or_default(),
                app: cf_text(CFDictionaryGetValue(dictionary, kCGWindowOwnerName))
                    .unwrap_or_default(),
                region,
            });
        }
    }
    Ok((windows, truncated))
}
fn windows() -> Result<Value, String> {
    let (windows, truncated) = native_windows()?;
    let items: Vec<_> = windows
        .into_iter()
        .map(|window| {
            json!({
                "window_id":window.id.to_string(),"pid":window.pid,
                "title":bounded(&window.title,512),"app":bounded(&window.app,128),
                "x":window.region.x,"y":window.region.y,
                "width":window.region.width,"height":window.region.height,
            })
        })
        .collect();
    Ok(json!({
        "windows":items,"truncated":truncated,"coordinate_space":"desktop_logical",
        "metadata_may_be_redacted":!unsafe { CGPreflightScreenCaptureAccess() },
    }))
}

fn accessibility() -> Result<(), String> {
    // Probe only: never call AXIsProcessTrustedWithOptions with prompt=true.
    if unsafe { AXIsProcessTrusted() } == 0 {
        Err(ACCESSIBILITY_REASON.into())
    } else {
        Ok(())
    }
}
fn input_ready() -> Result<(), String> {
    if !unsafe { CGPreflightPostEventAccess() } {
        return Err(POST_EVENT_REASON.into());
    }
    // A display alone may exist outside the interactive login security session.
    let _ = native_windows()?;
    Ok(())
}
fn status() -> Value {
    let geometry = monitors();
    let desktop_windows = native_windows();
    let graphical = geometry.is_ok() && desktop_windows.is_ok();
    let access = unsafe { AXIsProcessTrusted() } != 0;
    let post = unsafe { CGPreflightPostEventAccess() };
    let recording = unsafe { CGPreflightScreenCaptureAccess() };
    let capture = Path::new(SCREEN_CAPTURE).is_file() && Path::new(IMAGE_PROCESSOR).is_file();
    let session_reason = geometry
        .as_ref()
        .err()
        .or(desktop_windows.as_ref().err())
        .map(String::as_str);
    let input_reason = if !post {
        Some(POST_EVENT_REASON)
    } else {
        session_reason
    };
    let screenshot_reason = if !recording {
        Some(SCREEN_REASON)
    } else if !capture {
        Some("Apple screencapture or sips is unavailable at its standard system path.")
    } else {
        session_reason
    };
    json!({
        "platform":"macos","session":"quartz","native":true,
        "available":graphical,"coordinate_space":"desktop_logical",
        "coordinate_origin":"upper-left of main display; other displays can have negative x or y",
        "permissions":{
            "accessibility":{"granted":access,"reason":if access {None}else{Some(ACCESSIBILITY_REASON)}},
            "event_posting":{"granted":post,"reason":if post {None}else{Some(POST_EVENT_REASON)}},
            "screen_recording":{"granted":recording,"reason":if recording {None}else{Some(SCREEN_REASON)}}
        },
        "helpers":{
            "screencapture":{"installed":Path::new(SCREEN_CAPTURE).is_file(),"path":SCREEN_CAPTURE},
            "sips":{"installed":Path::new(IMAGE_PROCESSOR).is_file(),"path":IMAGE_PROCESSOR}
        },
        "capabilities":{
            "monitors":capability(graphical,"CoreGraphics",session_reason),
            "windows":capability(graphical,"Quartz Window Services",session_reason),
            "focus":capability(graphical&&access,"Accessibility",if !access {Some(ACCESSIBILITY_REASON)}else{session_reason}),
            "screenshot":capability(graphical&&recording&&capture,"Apple screencapture + sips",screenshot_reason),
            "move":capability(graphical&&post,"Quartz Events",input_reason),
            "click":capability(graphical&&post,"Quartz Events",input_reason),
            "drag":capability(graphical&&post,"Quartz Events",input_reason),
            "key":capability(graphical&&post,"Quartz Events",input_reason),
            "type":capability(graphical&&post,"Quartz Unicode Events",input_reason),
            "scroll":capability(graphical&&post,"Quartz Events",input_reason)
        }
    })
}

fn ax_attribute(element: Cf, name: &str) -> Result<Owned, String> {
    let name = cf_string(name)?;
    let mut value = ptr::null();
    // SAFETY: element is a retained AX object; Copy initializes value on success.
    let error = unsafe { AXUIElementCopyAttributeValue(element, name.0, &mut value) };
    if error != 0 {
        return Err(format!("Accessibility attribute query failed ({error})"));
    }
    Owned::new(value, "AXUIElementCopyAttributeValue")
}
fn ax_application(pid: i32) -> Result<Owned, String> {
    let app = Owned::new(
        unsafe { AXUIElementCreateApplication(pid) },
        "AX application",
    )?;
    // Every AX IPC has an explicit timeout; list walks additionally have a deadline.
    let error = unsafe { AXUIElementSetMessagingTimeout(app.0, 0.1) };
    if error != 0 {
        return Err(format!("Could not bound Accessibility messaging ({error})"));
    }
    Ok(app)
}
fn ax_region(window: Cf) -> Result<Region, String> {
    let position = ax_attribute(window, "AXPosition")?;
    let size = ax_attribute(window, "AXSize")?;
    let mut point = CgPoint::default();
    let mut extent = CgSize::default();
    // SAFETY: both AXValue objects and requested CGPoint/CGSize value kinds match.
    unsafe {
        if CFGetTypeID(position.0) != AXValueGetTypeID()
            || CFGetTypeID(size.0) != AXValueGetTypeID()
            || !AXValueGetValue(position.0, 1, (&mut point as *mut CgPoint).cast())
            || !AXValueGetValue(size.0, 2, (&mut extent as *mut CgSize).cast())
        {
            return Err("Accessibility window has no valid position and size".into());
        }
    }
    region_from_rect(CgRect {
        origin: point,
        size: extent,
    })
}
fn region_matches(a: Region, b: Region) -> bool {
    a.x.abs_diff(b.x) <= 2
        && a.y.abs_diff(b.y) <= 2
        && a.width.abs_diff(b.width) <= 2
        && a.height.abs_diff(b.height) <= 2
}
fn ax_matches(window: Cf, target: &Window) -> bool {
    if !ax_region(window).is_ok_and(|region| region_matches(region, target.region)) {
        return false;
    }
    let title = ax_attribute(window, "AXTitle")
        .ok()
        .and_then(|value| cf_text(value.0));
    target.title.is_empty() || title.is_none_or(|value| value == target.title)
}
fn focus_once(target: &Window) -> Result<(), String> {
    accessibility()?;
    let app = ax_application(target.pid)?;
    let list = ax_attribute(app.0, "AXWindows")?;
    if unsafe { CFGetTypeID(list.0) } != unsafe { CFArrayGetTypeID() } {
        return Err("Accessibility returned no window array".into());
    }
    let count = unsafe { CFArrayGetCount(list.0) };
    if !(0..=512).contains(&count) {
        return Err("Accessibility returned an unsupported window count".into());
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut found = None;
    for index in 0..count {
        if Instant::now() >= deadline {
            return Err("Accessibility window matching exceeded its 3 second deadline".into());
        }
        let window = unsafe { CFArrayGetValueAtIndex(list.0, index) };
        if window.is_null() {
            continue;
        }
        let error = unsafe { AXUIElementSetMessagingTimeout(window, 0.1) };
        if error != 0 {
            continue;
        }
        if ax_matches(window, target) {
            if found.is_some() {
                return Err(
                    "Window identity is ambiguous; refresh computer.windows before focusing".into(),
                );
            }
            found = Some(Owned::retained(window)?);
        }
    }
    let selected = found.ok_or(
        "Window no longer matches its observed identity, or the application exposes no Accessibility window; refresh computer.windows"
    )?;
    let frontmost = cf_string("AXFrontmost")?;
    let raise = cf_string("AXRaise")?;
    // The focus action is sent once. Verification below never repeats it.
    let activate = unsafe { AXUIElementSetAttributeValue(app.0, frontmost.0, kCFBooleanTrue) };
    if activate != 0 {
        return Err(format!(
            "Accessibility could not activate the application ({activate})"
        ));
    }
    let raised = unsafe { AXUIElementPerformAction(selected.0, raise.0) };
    if raised != 0 {
        return Err(format!(
            "Accessibility could not raise the selected window ({raised})"
        ));
    }
    // Some apps expose AXFocusedWindow as read-only. Raising is the required action;
    // supported setters improve focus, and the actual result is always checked.
    let focused = cf_string("AXFocusedWindow")?;
    let main = cf_string("AXMain")?;
    unsafe {
        AXUIElementSetAttributeValue(app.0, focused.0, selected.0);
        AXUIElementSetAttributeValue(selected.0, main.0, kCFBooleanTrue);
    }
    Ok(())
}
fn focused_matches(target: &Window) -> Result<bool, String> {
    let app = ax_application(target.pid)?;
    let frontmost = ax_attribute(app.0, "AXFrontmost")?;
    if cf_bool(frontmost.0) != Some(true) {
        return Ok(false);
    }
    let focused = ax_attribute(app.0, "AXFocusedWindow")?;
    let error = unsafe { AXUIElementSetMessagingTimeout(focused.0, 0.1) };
    if error != 0 {
        return Err(format!(
            "Could not bound focused-window verification ({error})"
        ));
    }
    let (windows, _) = native_windows()?;
    let Some(current) = windows
        .iter()
        .find(|w| w.id == target.id && w.pid == target.pid)
    else {
        return Err("Window disappeared while verifying focus".into());
    };
    Ok(ax_matches(focused.0, current))
}
fn parse_window_id(value: &str) -> Result<u32, String> {
    if value.is_empty() || value.len() > 10 || !value.bytes().all(|b| b.is_ascii_digit()) {
        return Err("window_id must be a numeric ID returned by computer.windows".into());
    }
    value
        .parse::<u32>()
        .ok()
        .filter(|v| *v != 0)
        .ok_or_else(|| "window_id is outside the native window ID range".into())
}
async fn focus(value: Focus) -> Result<(), String> {
    let id = parse_window_id(&value.window_id)?;
    let (windows, _) = native_windows()?;
    let target = windows
        .into_iter()
        .find(|w| w.id == id)
        .ok_or("Window is not currently visible; refresh computer.windows")?;
    focus_once(&target)?;
    let deadline = Instant::now() + Duration::from_millis(500);
    loop {
        let matched = focused_matches(&target)?;
        if Instant::now() >= deadline {
            return Err(
                "macOS did not confirm the requested foreground window within 500 ms".into(),
            );
        }
        if matched {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn validate_point(point: Point, displays: &[Monitor]) -> Result<(), String> {
    point.validate()?;
    if !displays.iter().any(|m| {
        Region {
            x: m.x,
            y: m.y,
            width: m.width,
            height: m.height,
        }
        .contains(point)
    }) {
        return Err("point is outside active monitors".into());
    }
    Ok(())
}
fn cursor() -> Result<Point, String> {
    let event = Owned::new(unsafe { CGEventCreate(ptr::null()) }, "CGEventCreate")?;
    let position = unsafe { CGEventGetLocation(event.0) };
    if !position.x.is_finite()
        || !position.y.is_finite()
        || position.x.abs() > 100000.0
        || position.y.abs() > 100000.0
    {
        return Err("macOS returned an invalid cursor position".into());
    }
    Point {
        x: position.x.round() as i32,
        y: position.y.round() as i32,
    }
    .validate()
}
fn button_id(button: Button) -> u32 {
    match button {
        Button::Left => 0,
        Button::Right => 1,
        Button::Middle => 2,
    }
}
fn mouse_kind(button: Button, down: bool) -> u32 {
    match (button, down) {
        (Button::Left, true) => 1,
        (Button::Left, false) => 2,
        (Button::Right, true) => 3,
        (Button::Right, false) => 4,
        (Button::Middle, true) => 25,
        (Button::Middle, false) => 26,
    }
}
fn mouse_event(kind: u32, button: Button, point: Point, count: u8) -> Result<(), String> {
    let event = Owned::new(
        unsafe {
            CGEventCreateMouseEvent(
                ptr::null(),
                kind,
                CgPoint {
                    x: f64::from(point.x),
                    y: f64::from(point.y),
                },
                button_id(button),
            )
        },
        "CGEventCreateMouseEvent",
    )?;
    unsafe {
        // kCGMouseEventClickState.
        CGEventSetIntegerValueField(event.0.cast_mut(), 1, i64::from(count));
        CGEventPost(0, event.0);
    }
    Ok(())
}
fn keyboard_event(
    code: u16,
    down: bool,
    flags: u64,
    unicode: Option<&[u16]>,
) -> Result<(), String> {
    let event = Owned::new(
        unsafe { CGEventCreateKeyboardEvent(ptr::null(), code, down) },
        "CGEventCreateKeyboardEvent",
    )?;
    unsafe {
        CGEventSetFlags(event.0.cast_mut(), flags);
        if let Some(text) = unicode {
            CGEventKeyboardSetUnicodeString(event.0.cast_mut(), text.len(), text.as_ptr());
        }
        CGEventPost(0, event.0);
    }
    Ok(())
}

/// Only numeric state crosses an await. Drop retries releases synchronously; no
/// helper thread or detached task can continue injecting input after cancellation.
#[derive(Default)]
struct InputRelease {
    mouse: Option<(Button, Point, u8)>,
    key: Option<u16>,
    modifiers: Vec<(u16, u64)>,
    base_flags: u64,
}
impl InputRelease {
    fn flags(&self) -> u64 {
        self.modifiers
            .iter()
            .fold(self.base_flags, |flags, (_, mask)| flags | mask)
    }
    fn finish(&mut self) -> Result<(), String> {
        if let Some((button, point, count)) = self.mouse {
            mouse_event(mouse_kind(button, false), button, point, count)?;
            self.mouse = None;
        }
        if let Some(code) = self.key {
            keyboard_event(code, false, self.flags(), None)?;
            self.key = None;
        }
        while let Some((code, _)) = self.modifiers.last().copied() {
            let flags = self.modifiers[..self.modifiers.len() - 1]
                .iter()
                .fold(self.base_flags, |flags, (_, mask)| flags | mask);
            keyboard_event(code, false, flags, None)?;
            self.modifiers.pop();
        }
        Ok(())
    }
}
impl Drop for InputRelease {
    fn drop(&mut self) {
        if let Some((button, point, count)) = self.mouse.take() {
            let _ = mouse_event(mouse_kind(button, false), button, point, count);
        }
        if let Some(code) = self.key.take() {
            let _ = keyboard_event(code, false, self.flags(), None);
        }
        while let Some((code, _)) = self.modifiers.pop() {
            let _ = keyboard_event(code, false, self.flags(), None);
        }
    }
}

fn move_pointer(point: Point) -> Result<(), String> {
    mouse_event(5, Button::Left, point, 0)
}
async fn click(value: Click) -> Result<(), String> {
    if !(1..=3).contains(&value.count) {
        return Err("count must be between 1 and 3".into());
    }
    let displays = monitors()?;
    let point = optional_point(value.x, value.y)?.map_or_else(cursor, Ok)?;
    validate_point(point, &displays)?;
    input_ready()?;
    if unsafe { CGEventSourceButtonState(0, button_id(value.button)) } {
        return Err(
            "The requested mouse button is already held; release it before sending a click".into(),
        );
    }
    move_pointer(point)?;
    for count in 1..=value.count {
        let mut release = InputRelease {
            mouse: Some((value.button, point, count)),
            ..InputRelease::default()
        };
        mouse_event(mouse_kind(value.button, true), value.button, point, count)?;
        tokio::time::sleep(Duration::from_millis(25)).await;
        release.finish()?;
        if count < value.count {
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
    }
    Ok(())
}
async fn drag(value: Drag) -> Result<(), String> {
    if !(2..=64).contains(&value.path.len()) || value.duration_ms > 5000 {
        return Err("drag requires 2–64 points and duration_ms at most 5000".into());
    }
    let displays = monitors()?;
    for point in &value.path {
        validate_point(*point, &displays)?;
    }
    input_ready()?;
    if unsafe { CGEventSourceButtonState(0, button_id(value.button)) } {
        return Err(
            "The requested mouse button is already held; release it before dragging".into(),
        );
    }
    let start = value.path[0];
    move_pointer(start)?;
    let mut release = InputRelease {
        mouse: Some((value.button, start, 1)),
        ..InputRelease::default()
    };
    mouse_event(mouse_kind(value.button, true), value.button, start, 1)?;
    let kind = match value.button {
        Button::Left => 6,
        Button::Right => 7,
        Button::Middle => 27,
    };
    for point in &value.path[1..] {
        if value.duration_ms > 0 {
            tokio::time::sleep(Duration::from_millis(
                value.duration_ms / (value.path.len() - 1) as u64,
            ))
            .await;
        }
        // Store the last requested position before injecting: cancellation or an
        // event allocation error still releases at this position.
        release.mouse = Some((value.button, *point, 1));
        mouse_event(kind, value.button, *point, 1)?;
    }
    release.finish()
}

fn keycode(key: &str) -> Result<u16, String> {
    // Public XKB-style identifiers map to the corresponding ANSI virtual key;
    // arbitrary Unicode text belongs to computer.type, not a keyboard layout guess.
    let name = key.to_ascii_lowercase();
    let code = match name.as_str() {
        "a" => 0,
        "s" => 1,
        "d" => 2,
        "f" => 3,
        "h" => 4,
        "g" => 5,
        "z" => 6,
        "x" => 7,
        "c" => 8,
        "v" => 9,
        "b" => 11,
        "q" => 12,
        "w" => 13,
        "e" => 14,
        "r" => 15,
        "y" => 16,
        "t" => 17,
        "1" => 18,
        "2" => 19,
        "3" => 20,
        "4" => 21,
        "6" => 22,
        "5" => 23,
        "equal" => 24,
        "9" => 25,
        "7" => 26,
        "minus" => 27,
        "8" => 28,
        "0" => 29,
        "bracketright" => 30,
        "o" => 31,
        "u" => 32,
        "bracketleft" => 33,
        "i" => 34,
        "p" => 35,
        "return" | "enter" => 36,
        "l" => 37,
        "j" => 38,
        "apostrophe" => 39,
        "k" => 40,
        "semicolon" => 41,
        "backslash" => 42,
        "comma" => 43,
        "slash" => 44,
        "n" => 45,
        "m" => 46,
        "period" => 47,
        "tab" => 48,
        "space" => 49,
        "grave" => 50,
        "backspace" => 51,
        "escape" | "esc" => 53,
        "super_l" | "command_l" => 55,
        "shift_l" => 56,
        "caps_lock" => 57,
        "alt_l" | "option_l" => 58,
        "control_l" => 59,
        "shift_r" => 60,
        "alt_r" | "option_r" => 61,
        "control_r" => 62,
        "super_r" | "command_r" => 54,
        "f17" => 64,
        "kp_decimal" => 65,
        "kp_multiply" => 67,
        "kp_add" => 69,
        "num_lock" | "clear" => 71,
        "kp_divide" => 75,
        "kp_enter" => 76,
        "kp_subtract" => 78,
        "f18" => 79,
        "f19" => 80,
        "kp_equal" => 81,
        "kp_0" => 82,
        "kp_1" => 83,
        "kp_2" => 84,
        "kp_3" => 85,
        "kp_4" => 86,
        "kp_5" => 87,
        "kp_6" => 88,
        "kp_7" => 89,
        "f20" => 90,
        "kp_8" => 91,
        "kp_9" => 92,
        "f5" => 96,
        "f6" => 97,
        "f7" => 98,
        "f3" => 99,
        "f8" => 100,
        "f9" => 101,
        "f11" => 103,
        "f13" => 105,
        "f16" => 106,
        "f14" => 107,
        "f10" => 109,
        "f12" => 111,
        "f15" => 113,
        "insert" | "help" => 114,
        "home" => 115,
        "prior" | "page_up" | "pageup" => 116,
        "delete" => 117,
        "f4" => 118,
        "end" => 119,
        "f2" => 120,
        "next" | "page_down" | "pagedown" => 121,
        "f1" => 122,
        "left" => 123,
        "right" => 124,
        "down" => 125,
        "up" => 126,
        _ => {
            return Err(format!(
                "Unsupported macOS key identifier: {}",
                bounded(key, 64)
            ));
        }
    };
    Ok(code)
}
fn primary_modifier_mask(code: u16) -> u64 {
    match code {
        54 | 55 => COMMAND,
        56 | 60 => SHIFT,
        58 | 61 => OPTION,
        59 | 62 => CONTROL,
        _ => 0,
    }
}
fn modifier(name: &str) -> Result<(u16, u64), String> {
    match name {
        "ctrl" => Ok((59, CONTROL)),
        "alt" => Ok((58, OPTION)),
        "shift" => Ok((56, SHIFT)),
        "super" => Ok((55, COMMAND)),
        _ => Err("modifiers must be distinct ctrl, alt, shift or super values".into()),
    }
}
async fn key(value: Key) -> Result<(), String> {
    if value.key.is_empty()
        || value.key.len() > 64
        || !value
            .key
            .bytes()
            .all(|v| v.is_ascii_alphanumeric() || v == b'_')
    {
        return Err("key must be an XKB-style name using letters, digits or underscores".into());
    }
    if value.modifiers.len() > 4 {
        return Err("at most four modifiers are supported".into());
    }
    let code = keycode(&value.key)?;
    let mut requested = Vec::new();
    for name in &value.modifiers {
        let item = modifier(name)?;
        if requested.contains(&item) {
            return Err("modifiers must be distinct ctrl, alt, shift or super values".into());
        }
        requested.push(item);
    }
    input_ready()?;
    if unsafe { CGEventSourceKeyState(0, code) } {
        return Err(
            "The requested key is already held; release it before sending the shortcut".into(),
        );
    }
    let mut release = InputRelease {
        base_flags: unsafe { CGEventSourceFlagsState(0) },
        ..InputRelease::default()
    };
    for (modifier_code, mask) in requested {
        if modifier_code == code || unsafe { CGEventSourceKeyState(0, modifier_code) } {
            continue;
        }
        release.modifiers.push((modifier_code, mask));
        keyboard_event(modifier_code, true, release.flags(), None)?;
    }
    release.key = Some(code);
    keyboard_event(
        code,
        true,
        release.flags() | primary_modifier_mask(code),
        None,
    )?;
    tokio::time::sleep(Duration::from_millis(20)).await;
    release.finish()
}
fn unicode_chunks(text: &str) -> Vec<Vec<u16>> {
    let mut chunks = Vec::new();
    let mut chunk = Vec::with_capacity(20);
    for ch in text.chars() {
        let mut scratch = [0_u16; 2];
        let units = ch.encode_utf16(&mut scratch);
        if chunk.len() + units.len() > 20 {
            chunks.push(std::mem::replace(&mut chunk, Vec::with_capacity(20)));
        }
        chunk.extend_from_slice(units);
    }
    if !chunk.is_empty() {
        chunks.push(chunk);
    }
    chunks
}
async fn type_text(value: TypeText) -> Result<Value, String> {
    if value.text.len() > 16384 || value.text.contains('\0') {
        return Err("text must be at most 16384 UTF-8 bytes without NUL".into());
    }
    if !value.text.is_empty() {
        input_ready()?;
        if unsafe { CGEventSourceFlagsState(0) } & MODIFIER_MASK != 0 {
            return Err(
                "A modifier key is currently held; release it before Unicode text input".into(),
            );
        }
        for chunk in unicode_chunks(&value.text) {
            if unsafe { CGEventSourceKeyState(0, 0) } {
                return Err(
                    "The ANSI A key is currently held; release it before Unicode text input".into(),
                );
            }
            let mut release = InputRelease {
                key: Some(0),
                ..InputRelease::default()
            };
            keyboard_event(0, true, 0, Some(&chunk))?;
            keyboard_event(0, false, 0, Some(&chunk))?;
            release.key = None;
            tokio::task::yield_now().await;
        }
    }
    Ok(result(
        json!({"typed":true,"characters":value.text.chars().count()}),
    ))
}
fn scroll_axes(direction: &str, steps: u8) -> Result<(i32, i32), String> {
    if !(1..=100).contains(&steps) {
        return Err("steps must be between 1 and 100".into());
    }
    let steps = i32::from(steps);
    match direction {
        "up" => Ok((steps, 0)),
        "down" => Ok((-steps, 0)),
        "left" => Ok((0, steps)),
        "right" => Ok((0, -steps)),
        _ => Err("direction must be up, down, left or right".into()),
    }
}
fn scroll(value: Scroll) -> Result<(), String> {
    let (vertical, horizontal) = scroll_axes(&value.direction, value.steps)?;
    let point = optional_point(value.x, value.y)?;
    if let Some(point) = point {
        validate_point(point, &monitors()?)?;
    }
    input_ready()?;
    if let Some(point) = point {
        move_pointer(point)?;
    }
    // kCGScrollEventUnitLine=1; two signed 32-bit wheel deltas.
    let event = Owned::new(
        unsafe { CGEventCreateScrollWheelEvent(ptr::null(), 1, 2, vertical, horizontal) },
        "CGEventCreateScrollWheelEvent",
    )?;
    unsafe { CGEventPost(0, event.0) };
    Ok(())
}

struct CaptureDirectory(PathBuf);
impl CaptureDirectory {
    fn new() -> Result<Self, String> {
        let mut entropy = [0_u8; 16];
        getrandom::fill(&mut entropy)
            .map_err(|e| format!("capture temporary directory entropy: {e}"))?;
        let token: String = entropy.iter().map(|b| format!("{b:02x}")).collect();
        let path =
            std::env::temp_dir().join(format!("ssmcp-computer-use-{}-{token}", std::process::id()));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .map_err(|e| format!("create private capture directory: {e}"))?;
        Ok(Self(path))
    }
}
impl Drop for CaptureDirectory {
    fn drop(&mut self) {
        // This path is freshly created with a random name and owner-only mode.
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn capture_args(region: Region, cursor: bool, path: &Path) -> Vec<String> {
    let mut args = vec![
        "-x".into(),
        "-t".into(),
        "jpg".into(),
        "-R".into(),
        format!(
            "{},{},{},{}",
            region.x, region.y, region.width, region.height
        ),
    ];
    if cursor {
        args.push("-C".into());
    }
    args.push(path.to_string_lossy().into_owned());
    args
}
async fn read_jpeg(path: &Path) -> Result<Vec<u8>, String> {
    let metadata = tokio::fs::symlink_metadata(path)
        .await
        .map_err(|e| format!("capture file: {e}"))?;
    if !metadata.file_type().is_file() || metadata.len() > CAPTURE_LIMIT as u64 {
        return Err("capture output is not a regular JPEG within the 16 MiB limit".into());
    }
    let file = tokio::fs::File::open(path)
        .await
        .map_err(|e| e.to_string())?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take((CAPTURE_LIMIT + 1) as u64)
        .read_to_end(&mut bytes)
        .await
        .map_err(|e| e.to_string())?;
    if bytes.len() > CAPTURE_LIMIT {
        return Err("capture exceeded 16 MiB".into());
    }
    jpeg_dimensions(&bytes)?;
    Ok(bytes)
}
fn validate_capture_budget(region: Region, displays: &[Monitor]) -> Result<(), String> {
    let scale_x = displays
        .iter()
        .map(|m| f64::from(m.pixel_width) / f64::from(m.width))
        .fold(1.0_f64, f64::max);
    let scale_y = displays
        .iter()
        .map(|m| f64::from(m.pixel_height) / f64::from(m.height))
        .fold(1.0_f64, f64::max);
    let width = (f64::from(region.width) * scale_x).ceil();
    let height = (f64::from(region.height) * scale_y).ceil();
    // screencapture captures before sips can resize. Account for the highest
    // display scale and empty space between monitors before invoking the helper.
    if !width.is_finite()
        || !height.is_finite()
        || width > MAX_CAPTURE_DIMENSION
        || height > MAX_CAPTURE_DIMENSION
        || width * height > MAX_CAPTURE_PIXELS
    {
        return Err("capture would exceed the 64 megapixel / 32768-pixel-side native capture budget; choose a smaller region".into());
    }
    Ok(())
}

async fn screenshot(root: &Path, value: Screenshot) -> Result<Value, String> {
    if !(320..=2560).contains(&value.max_width) || !(30..=90).contains(&value.quality) {
        return Err("max_width must be 320–2560; quality must be 30–90".into());
    }
    if !unsafe { CGPreflightScreenCaptureAccess() } {
        return Err(SCREEN_REASON.into());
    }
    let displays = monitors()?;
    let desktop = bounds(&displays)?;
    let region = value.region.unwrap_or(desktop).validate()?;
    if i64::from(region.x) < i64::from(desktop.x)
        || i64::from(region.y) < i64::from(desktop.y)
        || i64::from(region.x) + i64::from(region.width)
            > i64::from(desktop.x) + i64::from(desktop.width)
        || i64::from(region.y) + i64::from(region.height)
            > i64::from(desktop.y) + i64::from(desktop.height)
    {
        return Err("capture region is outside desktop bounds".into());
    }
    validate_capture_budget(region, &displays)?;
    let directory = CaptureDirectory::new()?;
    let original = directory.0.join("capture.jpg");
    let resized = directory.0.join("bounded.jpg");
    run_helper(
        root,
        SCREEN_CAPTURE,
        &capture_args(region, value.cursor, &original),
        None,
        TEXT_LIMIT,
        TIMEOUT,
    )
    .await?;
    let original_bytes = read_jpeg(&original).await?;
    let (original_width, _) = jpeg_dimensions(&original_bytes)?;
    let mut width = original_width.min(value.max_width);
    for _ in 0..5 {
        let args = vec![
            "--resampleWidth".into(),
            width.to_string(),
            "-s".into(),
            "format".into(),
            "jpeg".into(),
            "-s".into(),
            "formatOptions".into(),
            value.quality.to_string(),
            original.to_string_lossy().into_owned(),
            "--out".into(),
            resized.to_string_lossy().into_owned(),
        ];
        run_helper(root, IMAGE_PROCESSOR, &args, None, TEXT_LIMIT, TIMEOUT).await?;
        let bytes = read_jpeg(&resized).await?;
        let (actual_width, actual_height) = jpeg_dimensions(&bytes)?;
        if bytes.len() <= IMAGE_LIMIT {
            let metadata = json!({
                "mime_type":"image/jpeg","width":actual_width,"height":actual_height,"bytes":bytes.len(),
                "coordinate_space":"desktop_logical","region":region,
                "image_to_desktop":{
                    "origin_x":region.x,"origin_y":region.y,
                    "scale_x":f64::from(region.width)/f64::from(actual_width),
                    "scale_y":f64::from(region.height)/f64::from(actual_height)
                },
                "cursor":value.cursor,"backend":"Apple screencapture + sips"
            });
            return Ok(json!({
                "content":[{"type":"text","text":metadata.to_string()},
                    {"type":"image","mimeType":"image/jpeg","data":STANDARD.encode(bytes)}],
                "structuredContent":metadata,"isError":false
            }));
        }
        width = (width * 7 / 10).max(1);
    }
    Err("screenshot exceeds 400 KiB after bounded downscaling; select a smaller region".into())
}

/// Called under Computer's serial lock, closed check, and operation deadline.
pub(super) async fn perform(root: &Path, name: &str, args: Value) -> Result<Value, String> {
    match name {
        "ssmcp.computer.status" => {
            let _: Empty = parse(args)?;
            Ok(result(status()))
        }
        "ssmcp.computer.monitors" => {
            let _: Empty = parse(args)?;
            Ok(result(
                json!({"monitors":monitors()?,"coordinate_space":"desktop_logical"}),
            ))
        }
        "ssmcp.computer.windows" => {
            let _: Empty = parse(args)?;
            Ok(result(windows()?))
        }
        "ssmcp.computer.focus" => {
            let value: Focus = parse(args)?;
            let id = value.window_id.clone();
            focus(value).await?;
            Ok(result(json!({"focused":id})))
        }
        "ssmcp.computer.screenshot" => screenshot(root, parse(args)?).await,
        "ssmcp.computer.move" => {
            let point: Point = parse(args)?;
            validate_point(point, &monitors()?)?;
            input_ready()?;
            move_pointer(point)?;
            Ok(result(
                json!({"x":point.x,"y":point.y,"coordinate_space":"desktop_logical"}),
            ))
        }
        "ssmcp.computer.click" => {
            let value: Click = parse(args)?;
            let count = value.count;
            click(value).await?;
            Ok(result(json!({"clicked":true,"count":count})))
        }
        "ssmcp.computer.drag" => {
            drag(parse(args)?).await?;
            Ok(result(json!({"dragged":true,"button_released":true})))
        }
        "ssmcp.computer.key" => {
            key(parse(args)?).await?;
            Ok(result(json!({"pressed":true,"released":true})))
        }
        "ssmcp.computer.type" => type_text(parse(args)?).await,
        "ssmcp.computer.scroll" => {
            scroll(parse(args)?)?;
            Ok(result(json!({"scrolled":true})))
        }
        _ => Err(format!("unknown computer tool: {name}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quartz_key_names_preserve_command_and_navigation() {
        assert_eq!(keycode("a").unwrap(), 0);
        assert_eq!(keycode("A").unwrap(), 0);
        assert_eq!(keycode("Return").unwrap(), 36);
        assert_eq!(keycode("BackSpace").unwrap(), 51);
        assert_eq!(keycode("Delete").unwrap(), 117);
        assert_eq!(keycode("Page_Up").unwrap(), 116);
        assert_eq!(keycode("F20").unwrap(), 90);
        assert_eq!(modifier("super").unwrap(), (55, COMMAND));
        assert_eq!(modifier("ctrl").unwrap(), (59, CONTROL));
        assert!(keycode("arbitrary shell ; command").is_err());
        assert!(modifier("meta").is_err());
    }

    #[test]
    fn unicode_chunks_preserve_surrogate_pairs_and_line_breaks() {
        let text = format!("{}😀汉字\n\t𐐷", "x".repeat(19));
        let chunks = unicode_chunks(&text);
        assert!(
            chunks
                .iter()
                .all(|chunk| !chunk.is_empty() && chunk.len() <= 20)
        );
        assert!(chunks.iter().all(|chunk| String::from_utf16(chunk).is_ok()));
        assert_eq!(String::from_utf16(&chunks.concat()).unwrap(), text);
        assert!(unicode_chunks("").is_empty());
    }

    #[test]
    fn quartz_geometry_preserves_negative_origins_and_fractional_edges() {
        let region = region_from_rect(CgRect {
            origin: CgPoint {
                x: -1440.5,
                y: -30.25,
            },
            size: CgSize {
                width: 1440.0,
                height: 900.5,
            },
        })
        .unwrap();
        assert_eq!(
            (region.x, region.y, region.width, region.height),
            (-1441, -31, 1441, 902)
        );
        assert!(
            region_from_rect(CgRect {
                origin: CgPoint {
                    x: f64::NAN,
                    y: 0.0
                },
                size: CgSize {
                    width: 100.0,
                    height: 100.0
                }
            })
            .is_err()
        );
        assert!(
            region_from_rect(CgRect {
                origin: CgPoint { x: 0.0, y: 0.0 },
                size: CgSize {
                    width: 0.0,
                    height: 100.0
                }
            })
            .is_err()
        );
    }

    #[test]
    fn window_ids_reject_code_and_out_of_range_values() {
        assert_eq!(parse_window_id("4294967295").unwrap(), u32::MAX);
        for invalid in [
            "",
            "0",
            "-1",
            "4294967296",
            "1;open",
            "0x123",
            "12345678901",
        ] {
            assert!(parse_window_id(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn screenshot_arguments_keep_region_and_filename_as_distinct_argv() {
        let region = Region {
            x: -100,
            y: 20,
            width: 800,
            height: 600,
        };
        let args = capture_args(region, true, Path::new("/tmp/private capture/capture.jpg"));
        assert_eq!(
            args,
            vec![
                "-x",
                "-t",
                "jpg",
                "-R",
                "-100,20,800,600",
                "-C",
                "/tmp/private capture/capture.jpg"
            ]
        );
        assert_eq!(capture_args(region, false, Path::new("/tmp/a")).len(), 6);
    }

    #[test]
    fn capture_budget_bounds_retina_pixels_and_sparse_desktop_rectangles() {
        let displays = vec![Monitor {
            name: "Retina fixture".into(),
            x: -2560,
            y: 0,
            width: 2560,
            height: 1440,
            pixel_width: 5120,
            pixel_height: 2880,
            scale: 2.0,
            focused: true,
        }];
        assert!(
            validate_capture_budget(
                Region {
                    x: -2560,
                    y: 0,
                    width: 2560,
                    height: 1440
                },
                &displays
            )
            .is_ok()
        );
        assert!(
            validate_capture_budget(
                Region {
                    x: -50000,
                    y: -50000,
                    width: 100000,
                    height: 100000
                },
                &displays
            )
            .is_err()
        );
        assert!(
            validate_capture_budget(
                Region {
                    x: 0,
                    y: 0,
                    width: 10000,
                    height: 10000
                },
                &displays
            )
            .is_err()
        );
        assert!(
            validate_capture_budget(
                Region {
                    x: 0,
                    y: 0,
                    width: 20000,
                    height: 10
                },
                &displays
            )
            .is_err()
        );
    }

    #[test]
    fn scroll_deltas_are_bounded_and_have_quartz_direction() {
        assert_eq!(scroll_axes("up", 3).unwrap(), (3, 0));
        assert_eq!(scroll_axes("down", 3).unwrap(), (-3, 0));
        assert_eq!(scroll_axes("left", 3).unwrap(), (0, 3));
        assert_eq!(scroll_axes("right", 3).unwrap(), (0, -3));
        assert!(scroll_axes("up", 0).is_err());
        assert!(scroll_axes("right", 101).is_err());
        assert!(scroll_axes("diagonal", 3).is_err());
    }
}
