#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]
mod composer;
mod engine;
mod launcher;
mod model;
mod ui;

use model::Target;
use std::{
    fs::OpenOptions,
    io::Write,
    path::PathBuf,
    time::{Duration, Instant},
};
use zgpui::native::{Event, Ui, Window};

#[derive(Default)]
struct Options {
    target: Option<Target>,
    destination: Option<String>,
    control: bool,
    font: Option<String>,
    exit_after: Option<Duration>,
    first_frame_receipt: Option<PathBuf>,
    frame_receipts: Option<PathBuf>,
}

fn options() -> Result<Option<Options>, String> {
    let mut result = Options::default();
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--target") if result.destination.is_none() => {
                let target = args
                    .next()
                    .and_then(|v| v.into_string().ok())
                    .ok_or("--target 需要 用户名@中继服务器。")?;
                ssmcp::connection_form::validate_destination(&target)?;
                result.destination = Some(target);
            }
            Some("--session") => {
                if result.target.is_some() {
                    return Err("只指定一个 --session 或 --directory。".into());
                }
                let name = args
                    .next()
                    .and_then(|value| value.into_string().ok())
                    .filter(|value| !value.is_empty() && value.len() <= 128)
                    .ok_or("--session 需要已有会话名称。")?;
                result.target = Some(Target::Named(name));
            }
            Some("--directory") => {
                if result.target.is_some() {
                    return Err("只指定一个 --session 或 --directory。".into());
                }
                result.target = Some(Target::Directory(PathBuf::from(
                    args.next().ok_or("--directory 需要已有会话服务目录。")?,
                )));
            }
            Some("--control") => result.control = true,
            Some("--font") => {
                result.font = Some(
                    args.next()
                        .and_then(|value| value.into_string().ok())
                        .ok_or("--font 需要系统字体族名称。")?,
                )
            }
            Some("--exit-after-ms") => {
                let millis = args
                    .next()
                    .and_then(|value| value.into_string().ok())
                    .and_then(|value| value.parse::<u64>().ok())
                    .filter(|value| *value > 0 && *value <= 3_600_000)
                    .ok_or("--exit-after-ms 需要 1–3600000。")?;
                result.exit_after = Some(Duration::from_millis(millis));
            }
            Some("--first-frame-receipt") => {
                if result.first_frame_receipt.is_some() {
                    return Err("只指定一个 --first-frame-receipt。".into());
                }
                let path = args
                    .next()
                    .filter(|value| !value.is_empty())
                    .ok_or("--first-frame-receipt 需要一个不存在的文件路径。")?;
                result.first_frame_receipt = Some(PathBuf::from(path));
            }
            Some("--frame-receipts") => {
                if result.frame_receipts.is_some() {
                    return Err("只指定一个 --frame-receipts。".into());
                }
                let path = args
                    .next()
                    .filter(|value| !value.is_empty())
                    .ok_or("--frame-receipts 需要一个不存在的文件路径。")?;
                result.frame_receipts = Some(PathBuf::from(path));
            }
            Some("--help" | "-h") => {
                println!(
                    "ssmcp-desktop [--target USER@RELAY | --session NAME | --directory PATH] [--control] [--font FAMILY]\n\n无参数打开桌面连接界面；--session 或 --directory 可观察已有会话。\n--directory 使用显式提供的私有服务目录，不打印其内容。\n--exit-after-ms N 用于真实窗口的有界构建验收。\n--first-frame-receipt PATH 与有界验收一起使用，首次 present 返回后新建回执文件。\n--frame-receipts PATH 记录最多 4096 次 present 回执，用于尺寸变化后的真实窗口验收。"
                );
                return Ok(None);
            }
            _ => return Err("未知启动参数。使用 --help 查看可用选项。".into()),
        }
    }
    if result.target.is_some() && result.destination.is_some() {
        return Err("--target 与已有会话参数互斥。".into());
    }
    if result.control && result.target.is_none() {
        return Err("--control 需要明确指定 --session 或 --directory。".into());
    }
    if result.first_frame_receipt.is_some() && result.exit_after.is_none() {
        return Err("--first-frame-receipt 需要 --exit-after-ms。".into());
    }
    if result.frame_receipts.is_some() && result.exit_after.is_none() {
        return Err("--frame-receipts 需要 --exit-after-ms。".into());
    }
    Ok(Some(result))
}

fn run(options: Options) -> Result<(), String> {
    let mut window = Window::new("ssmcp-desktop", 1080, 720)?;
    let mut palette = zgpui::native::Palette::for_theme(zgpui::native::Theme::Dark);
    palette.background = 0x101113ff;
    palette.surface = 0x17181bff;
    palette.accent = 0xb6ceffff;
    palette.on_accent = 0x152138ff;
    window.set_palette(palette);
    let family = options
        .font
        .as_deref()
        .unwrap_or(if cfg!(target_os = "windows") {
            "Microsoft YaHei UI"
        } else {
            "Noto Sans CJK SC"
        });
    window.set_font_family(family)?;
    let mut handle = options
        .target
        .map(|target| engine::Handle::start(Some(target), options.control, None))
        .transpose()?;
    let mut launcher = launcher::Launcher::new(options.destination.as_deref());
    let mut persist_target: Option<String> = None;
    if handle.is_none()
        && let Some(target) = launcher.configured_target()
    {
        handle = Some(engine::Handle::start(None, true, Some(target))?);
    }
    let mut desktop = ui::Desktop::default();
    let mut ui = Ui::default();
    let started = Instant::now();
    let mut revision = u64::MAX;
    let mut snapshot = model::View::default();
    let mut pending_event = None;
    let mut dirty = true;
    let mut first_frame_receipt = options.first_frame_receipt;
    let mut frame_receipts = options
        .frame_receipts
        .map(|path| {
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
                .map_err(|error| format!("无法新建渲染帧回执：{error}"))
        })
        .transpose()?;
    let mut presented_frames = 0_u64;
    let mut presented_size = None;
    loop {
        if options
            .exit_after
            .is_some_and(|duration| started.elapsed() >= duration)
        {
            break;
        }
        if matches!(pending_event, Some(Event::Close)) {
            break;
        }
        let next_revision = handle.as_ref().map_or(0, engine::Handle::revision);
        if revision != next_revision {
            if let Some(handle) = &handle {
                snapshot = handle.snapshot();
                if snapshot.phase.connected()
                    && let Some(target) = persist_target.take()
                {
                    if let Err(e) = ssmcp::connection_form::save_desktop_profile(&target) {
                        desktop.notice(format!("连接已建立，无法保存启动设置：{e}"));
                    }
                }
                desktop.sync(&snapshot);
            }
            revision = next_revision;
            dirty = true;
        }
        // Native event coalescing can replace a resize notification with input.
        // Compare actual dimensions independently so every new size is drawn.
        if let Some(error) = snapshot.launch_error.take() {
            handle = None;
            launcher.show_error(error);
            ui = Ui::default();
            persist_target = None;
            snapshot = model::View::default();
            dirty = true;
        }
        if handle.is_none() {
            dirty |= launcher.poll_registration();
        }
        let size = window.size();
        dirty |= presented_size != Some(size);
        if dirty {
            let drew_form = handle.is_none();
            let event = pending_event.take();
            let consumed = event.as_ref().is_some_and(|event| {
                if handle.is_some() {
                    desktop.event(event)
                } else {
                    launcher.event(event)
                }
            });
            ui.begin(if consumed { None } else { event });
            let (width, height) = size;
            let mut canvas = window.begin(if handle.is_some() {
                ui::BG
            } else {
                launcher::BG
            });
            let mut submission = None;
            if let Some(handle) = &handle {
                desktop.draw(&mut canvas, &mut ui, handle, &snapshot, width, height);
            } else {
                submission = launcher.draw(&mut canvas, &mut ui, width, height);
            }
            ui.end();
            drop(canvas);
            window.present();
            if let Some(destination) = submission {
                handle = Some(engine::Handle::start(
                    None,
                    true,
                    Some(destination.clone()),
                )?);
                persist_target = Some(destination);
                revision = u64::MAX;
            }
            if desktop.take_setup() {
                handle = None;
                persist_target = None;
                desktop = ui::Desktop::default();
                snapshot = model::View::default();
                launcher = launcher::Launcher::new(None);
                ui = Ui::default();
                revision = u64::MAX;
            }
            presented_size = Some(size);
            presented_frames += 1;
            if first_frame_receipt.is_some() || frame_receipts.is_some() {
                // A present-call receipt is not proof of visible pixels.
                // Acceptance also inspects the real window at these dimensions.
                let mut receipt = serde_json::json!({
                    "schema": "ssmcp-desktop-present-v1",
                    "stage": "present_returned",
                    "pid": std::process::id(),
                    "frame_index": presented_frames,
                    "width": width,
                    "height": height,
                    "elapsed_ms": started.elapsed().as_millis(),
                });
                if let Some(path) = first_frame_receipt.take() {
                    receipt["schema"] = "ssmcp-desktop-first-frame-v1".into();
                    let bytes = serde_json::to_vec(&receipt)
                        .map_err(|error| format!("无法编码首帧回执：{error}"))?;
                    let mut file = OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(path)
                        .map_err(|error| format!("无法新建首帧回执：{error}"))?;
                    file.write_all(&bytes)
                        .map_err(|error| format!("无法写入首帧回执：{error}"))?;
                    receipt["schema"] = "ssmcp-desktop-present-v1".into();
                }
                if let Some(file) = frame_receipts.as_mut() {
                    if presented_frames > 4096 {
                        return Err("有界验收的渲染帧回执已达到 4096 条上限。".into());
                    }
                    serde_json::to_writer(&mut *file, &receipt)
                        .map_err(|error| format!("无法编码渲染帧回执：{error}"))?;
                    file.write_all(b"\n")
                        .and_then(|()| file.flush())
                        .map_err(|error| format!("无法写入渲染帧回执：{error}"))?;
                }
            }
            if let Some(text) = ui.clipboard.take().or_else(|| desktop.take_clipboard()) {
                window.copy(&text);
            }
            if drew_form
                && handle.is_none()
                && let Some(focus) = launcher.initial_focus()
            {
                pending_event = Some(focus);
                dirty = true;
                continue;
            }
        }
        pending_event = window.event(if handle.is_some() || options.exit_after.is_some() {
            150
        } else {
            1250
        });
        dirty = match &pending_event {
            Some(Event::Move(x, y)) => ui.pointer_changed(*x, *y),
            Some(_) => true,
            None => false,
        };
    }
    if let Some(handle) = &mut handle {
        handle.shutdown();
    }
    Ok(())
}

fn main() {
    let result = options().and_then(|options| options.map_or(Ok(()), run));
    if let Err(error) = result {
        // Startup errors contain application configuration/platform diagnostics,
        // never RemoteFrontend descriptor contents or authentication errors.
        eprintln!("ssmcp-desktop: {error}");
        std::process::exit(1);
    }
}
