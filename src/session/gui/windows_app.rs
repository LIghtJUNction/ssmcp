//! One system Edge app window and one loopback workbench per launch.
use std::{ffi::OsString, future::IntoFuture, path::PathBuf, time::Duration};
use tokio::{process::Command, time};
use windows_sys::Win32::{
    Foundation::HWND,
    UI::WindowsAndMessaging::{
        EnumWindows, GetClassNameW, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible,
    },
};

struct PrivateWindow(PathBuf);

impl Drop for PrivateWindow {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(self.0.join("open.html"));
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn edge_path() -> crate::Result<PathBuf> {
    [
        "ProgramFiles(x86)",
        "ProgramW6432",
        "ProgramFiles",
        "LOCALAPPDATA",
    ]
    .iter()
    .filter_map(std::env::var_os)
    .map(|root| PathBuf::from(root).join(r"Microsoft\Edge\Application\msedge.exe"))
    .find(|path| path.is_absolute() && path.is_file())
    .ok_or_else(|| "Microsoft Edge is required to open the SSMCP desktop window.".into())
}

fn app_window(process_id: u32) -> Option<isize> {
    struct Search {
        pid: u32,
        handle: Option<isize>,
    }
    unsafe extern "system" fn visit(window: HWND, data: isize) -> i32 {
        // EnumWindows is synchronous; the pointer lives until it returns.
        let search = unsafe { &mut *(data as *mut Search) };
        let mut pid = 0;
        let mut class = [0u16; 64];
        let mut title = [0u16; 96];
        unsafe {
            GetWindowThreadProcessId(window, &mut pid);
            if pid == search.pid && IsWindowVisible(window) != 0 {
                let length = GetClassNameW(window, class.as_mut_ptr(), class.len() as i32);
                if length > 0
                    && class[..length as usize]
                        .iter()
                        .copied()
                        .eq("Chrome_WidgetWin_1".encode_utf16())
                {
                    let len = GetWindowTextW(window, title.as_mut_ptr(), title.len() as i32);
                    if len > 0
                        && String::from_utf16_lossy(&title[..len as usize])
                            .to_ascii_lowercase()
                            .contains("ssmcp")
                    {
                        search.handle = Some(window as isize);
                        return 0;
                    }
                }
            }
        }
        1
    }
    let mut search = Search {
        pid: process_id,
        handle: None,
    };
    unsafe {
        EnumWindows(Some(visit), (&mut search as *mut Search) as isize);
    }
    search.handle
}

pub(super) async fn run(destination: Option<&str>) -> crate::Result<()> {
    let edge = edge_path()?;
    let parent = crate::session::state_path()?.join("gui").join("windows");
    crate::session::create_private_tree(&parent)?;
    crate::session::verify_directory(&parent)?;
    let path = parent.join(crate::session::random_suffix()?);
    crate::platform::private_directory(&path)?;
    let window = PrivateWindow(path);
    let (listener, app, url) = super::owned_workbench(destination).await?;

    // The public process command line contains only a private file path. The
    // loopback URL and its pairing fragment stay inside this user's directory.
    let launch = window.0.join("open.html");
    let entry = format!(
        "<!doctype html><meta charset=utf-8><meta name=referrer content=no-referrer><script>location.replace({})</script>",
        serde_json::to_string(url.as_str())?
    );
    crate::session::write_private(&launch, entry.as_bytes())?;
    let file_url =
        reqwest::Url::from_file_path(&launch).map_err(|_| "invalid desktop entry path")?;
    let mut profile = OsString::from("--user-data-dir=");
    profile.push(window.0.join("profile"));
    let mut child = Command::new(edge)
        .arg(profile)
        .args([
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-background-mode",
            "--disable-extensions",
        ])
        .arg(format!("--app={file_url}"))
        .kill_on_drop(true)
        .spawn()?;
    let pid = child.id().ok_or("the Edge application did not start")?;
    let lifetime = async {
        let deadline = time::Instant::now() + Duration::from_secs(45);
        let mut handle = None;
        loop {
            if let Some(status) = child.try_wait()? {
                return if handle.is_some() && status.success() {
                    Ok(())
                } else {
                    Err(format!("Edge exited before the desktop window closed ({status})").into())
                };
            }
            if let Some(hwnd) = handle {
                // Legal links and registration may open other top-level windows
                // in this profile. Only the original workbench owns the gateway.
                let mut owner = 0;
                unsafe {
                    GetWindowThreadProcessId(hwnd as HWND, &mut owner);
                }
                if owner != pid {
                    return Ok(());
                }
            } else {
                handle = app_window(pid);
                if handle.is_none() && time::Instant::now() >= deadline {
                    return Err(
                        "Microsoft Edge did not open its own SSMCP application window".into(),
                    );
                }
            }
            time::sleep(Duration::from_millis(200)).await;
        }
    };
    let result = tokio::select! {
        result = axum::serve(listener, app).into_future() => result.map_err(Into::into),
        result = lifetime => result,
    };
    if child.try_wait()?.is_none()
        && let Err(error) = child.kill().await
        && child.try_wait()?.is_none()
    {
        return Err(error.into());
    }
    // Chromium helpers can briefly hold files after the browser has exited.
    // Remove the private bootstrap first and allow the profile a bounded cleanup.
    let _ = std::fs::remove_file(&launch);
    for _ in 0..12 {
        if !window.0.exists() || std::fs::remove_dir_all(&window.0).is_ok() {
            break;
        }
        time::sleep(Duration::from_millis(100)).await;
    }
    result
}
