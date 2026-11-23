#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]
mod engine;
mod model;
mod ui;

use model::Target;
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};
use zgpui::native::{Event, Ui, Window};

#[derive(Default)]
struct Options {
    target: Option<Target>,
    control: bool,
    font: Option<String>,
    exit_after: Option<Duration>,
}

fn options() -> Result<Option<Options>, String> {
    let mut result = Options::default();
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        match arg.to_str() {
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
            Some("--help" | "-h") => {
                println!(
                    "ssmcp-desktop [--session NAME | --directory PATH] [--control] [--font FAMILY]\n\n原生 Windows / Linux 客户端；默认只观察已有的 CLI 会话。\n--directory 使用显式提供的私有服务目录，不打印其内容。\n--exit-after-ms N 用于真实窗口的有界构建验收。"
                );
                return Ok(None);
            }
            _ => return Err("未知启动参数。使用 --help 查看可用选项。".into()),
        }
    }
    if result.control && result.target.is_none() {
        return Err("--control 需要明确指定 --session 或 --directory。".into());
    }
    Ok(Some(result))
}

fn run(options: Options) -> Result<(), String> {
    let mut window = Window::new("ssmcp-desktop · 本地工作台", 1180, 800)?;
    let family = options
        .font
        .as_deref()
        .unwrap_or(if cfg!(target_os = "windows") {
            "Microsoft YaHei UI"
        } else {
            "Noto Sans CJK SC"
        });
    window.set_font_family(family)?;
    let mut handle = engine::Handle::start(options.target, options.control)?;
    let mut desktop = ui::Desktop::default();
    let mut ui = Ui::default();
    let started = Instant::now();
    let mut revision = u64::MAX;
    let mut snapshot = model::View::default();
    let mut pending_event = None;
    let mut dirty = true;
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
        let next_revision = handle.revision();
        if revision != next_revision {
            snapshot = handle.snapshot();
            desktop.sync(&snapshot);
            revision = next_revision;
            dirty = true;
        }
        if dirty {
            if let Some(event) = &pending_event {
                desktop.event(event);
            }
            ui.begin(pending_event.take());
            let (width, height) = window.size();
            let mut canvas = window.begin(ui::BG);
            desktop.draw(&mut canvas, &mut ui, &handle, &snapshot, width, height);
            ui.end();
            drop(canvas);
            window.present();
            if let Some(text) = ui.clipboard.take() {
                window.copy(&text);
            }
        }
        pending_event = window.event(150);
        dirty = match &pending_event {
            Some(Event::Move(x, y)) => ui.pointer_changed(*x, *y),
            Some(_) => true,
            None => false,
        };
    }
    handle.shutdown();
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
