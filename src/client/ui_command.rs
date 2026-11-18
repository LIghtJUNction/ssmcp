//! Local display commands. Preview uses fixture data and never starts a backend.
use super::ui::{Theme, Ui};
use std::{
    ffi::OsString,
    fs::OpenOptions,
    io::{self, Write},
    path::{Path, PathBuf},
    time::Duration,
};

const HELP: &str = r#"ssmcp ui — configure and preview your client

Usage:
  ssmcp ui show [--ui-config FILE] [--theme NAME]
  ssmcp ui check [--ui-config FILE]
  ssmcp ui init [--ui-config FILE]
  ssmcp ui preview [--ui-config FILE] [--theme NAME]
  ssmcp ui preview --static [--rows N] [--cols N] [--theme NAME]

Commands:
  show      Print the effective [ui] configuration as TOML
  check     Validate configuration without connecting or starting tools
  init      Create a default configuration; existing files are preserved
  preview   Try the real client display with sample data, without a connection

Options:
  --ui-config FILE  Override SSMCP_UI_CONFIG and the default config path
  --theme NAME     dark (default), minimal, codex-dark, nord, solarized, or light
  --static         Render one preview frame to stdout, including ANSI colors
  --rows N         Static frame height, 1..1000 (default: 36)
  --cols N         Static frame width, 1..1000 (default: 120)

Preview: F1 help; Ctrl-C or Ctrl-] exits. Changes stay in memory unless saved.
Default configuration: $XDG_CONFIG_HOME/ssmcp/config.toml, or ~/.config/ssmcp/config.toml.
NO_COLOR disables preview colors, including in static output.
"#;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Command {
    Show,
    Check,
    Init,
    Preview,
}

struct Options {
    command: Command,
    path: Option<PathBuf>,
    theme: Option<Theme>,
    static_frame: bool,
    rows: u16,
    cols: u16,
}

impl Options {
    fn parse(args: Vec<OsString>) -> Result<Option<Self>, String> {
        if args.is_empty() || args.iter().any(|arg| arg == "--help" || arg == "-h") {
            return Ok(None);
        }
        let mut args = args.into_iter();
        let command = match args
            .next()
            .and_then(|value| value.into_string().ok())
            .as_deref()
        {
            Some("show") => Command::Show,
            Some("check") => Command::Check,
            Some("init") => Command::Init,
            Some("preview") => Command::Preview,
            _ => return Err("unknown UI command; use ssmcp ui --help".into()),
        };
        let mut options = Self {
            command,
            path: None,
            theme: None,
            static_frame: false,
            rows: 36,
            cols: 120,
        };
        let mut custom_dimensions = false;
        while let Some(argument) = args.next() {
            match argument.to_str() {
                Some("--ui-config") => {
                    let path = args.next().ok_or("--ui-config requires a file")?;
                    if path.is_empty() {
                        return Err("--ui-config requires a nonempty file path".into());
                    }
                    options.path = Some(path.into());
                }
                Some("--theme") if matches!(command, Command::Show | Command::Preview) => {
                    let theme = args.next().ok_or("--theme requires a name")?;
                    options.theme =
                        Some(Theme::parse(theme.to_str().ok_or("theme must be UTF-8")?)?);
                }
                Some("--static") if command == Command::Preview => options.static_frame = true,
                Some("--rows" | "--cols") if command == Command::Preview => {
                    let value = args.next().ok_or("preview dimensions require a value")?;
                    let number = value
                        .to_str()
                        .and_then(|value| value.parse::<u16>().ok())
                        .filter(|number| (1..=1000).contains(number))
                        .ok_or("preview dimensions must be in 1..=1000")?;
                    if argument == "--rows" {
                        options.rows = number;
                    } else {
                        options.cols = number;
                    }
                    custom_dimensions = true;
                }
                _ => return Err("unsupported UI option; use ssmcp ui --help".into()),
            }
        }
        if custom_dimensions && !options.static_frame {
            return Err("--rows and --cols require preview --static".into());
        }
        Ok(Some(options))
    }
}

pub(crate) async fn run(args: Vec<OsString>) -> crate::Result<()> {
    let Some(options) = Options::parse(args)? else {
        print!("{HELP}");
        return Ok(());
    };
    if options.command == Command::Init {
        let path = Ui::config_path(options.path.as_deref())
            .ok_or("cannot determine a UI config path; use --ui-config FILE")?;
        initialize_file(&path)?;
        println!("Created UI configuration: {}", path.display());
        return Ok(());
    }
    let mut ui = Ui::load(options.path.as_deref())?;
    if let Some(theme) = options.theme {
        ui.theme = theme;
    }
    match options.command {
        Command::Show => print!("{}", ui.to_toml()?),
        Command::Check => {
            ui.validate()?;
            if let Some(path) =
                Ui::config_path(options.path.as_deref()).filter(|path| path.is_file())
            {
                println!("UI configuration valid: {}", path.display());
            } else {
                println!("UI configuration valid: built-in defaults (no configuration file)");
            }
        }
        Command::Preview => {
            ui.color &= std::env::var_os("NO_COLOR").is_none();
            if options.static_frame {
                let bytes = super::terminal::preview_bytes(&ui, options.rows, options.cols);
                let mut output = io::stdout().lock();
                output.write_all(&bytes)?;
                output.flush()?;
            } else {
                preview(ui, options.path.as_deref()).await?;
            }
        }
        Command::Init => unreachable!(),
    }
    Ok(())
}

fn initialize_file(path: &Path) -> crate::Result<()> {
    let contents = Ui::default().to_toml()?;
    if let Some(parent) = path.parent().filter(|path| !path.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            return Err(format!(
                "UI configuration already exists: {}; use ssmcp ui show or edit the file",
                path.display()
            )
            .into());
        }
        Err(error) => return Err(error.into()),
    };
    file.write_all(contents.as_bytes())?;
    file.sync_all()?;
    Ok(())
}

async fn preview(ui: Ui, explicit: Option<&Path>) -> crate::Result<()> {
    use crate::session::attach::input::{Event, Keyboard};
    super::interface::initialize_path(explicit);
    super::render::configure(ui.clone());
    let cwd = std::env::current_dir()?;
    let Some(_display) = super::terminal::Session::start(
        &ui,
        &cwd.to_string_lossy(),
        None,
        Some(super::account::DEFAULT_SERVER),
    )?
    else {
        return Err("interactive preview requires a foreground terminal with dashboard enabled and a pretty log format; use ssmcp ui preview --static for a single frame".into());
    };
    let mut keyboard = Keyboard::open(false)?;
    super::terminal::seed_preview();
    let mut tick = tokio::time::interval(Duration::from_millis(50));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            event = keyboard.next() => match event? {
                Event::Bytes(bytes) => {
                    let _ = super::terminal::keyboard_bytes(&bytes);
                }
                Event::Resize(rows, cols) => {
                    if rows > 0 && cols > 0 {
                        super::terminal::physical_resize(rows, cols);
                    }
                }
                Event::Interrupt | Event::Detach | Event::End => break,
            },
            _ = tick.tick() => {
                let _ = super::terminal::take_keyboard();
                if let Err(error) = super::interface::apply_pending() {
                    super::terminal::notice(&error);
                }
            }
        }
    }
    // Keyboard and compositor guards restore terminal mode on both success and errors.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Option<Options>, String> {
        Options::parse(args.iter().map(OsString::from).collect())
    }

    #[test]
    fn dimensions_are_bounded_and_only_apply_to_static_preview() {
        let options = parse(&[
            "preview", "--static", "--rows", "58", "--cols", "106", "--theme", "light",
        ])
        .unwrap()
        .unwrap();
        assert_eq!((options.rows, options.cols), (58, 106));
        assert_eq!(options.theme, Some(Theme::Light));
        for args in [
            vec!["preview", "--rows", "58"],
            vec!["preview", "--static", "--rows", "0"],
            vec!["preview", "--static", "--cols", "1001"],
            vec!["check", "--theme", "nord"],
            vec!["init", "--static"],
            vec!["show", "--ui-config", ""],
        ] {
            assert!(parse(&args).is_err());
        }
    }

    #[test]
    fn init_never_replaces_existing_configuration() {
        let directory = std::env::temp_dir().join(format!(
            "ssmcp-ui-init-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = directory.join("config.toml");
        initialize_file(&path).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert!(Ui::parse(std::str::from_utf8(&bytes).unwrap()).is_ok());
        assert!(initialize_file(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        std::fs::remove_dir_all(directory).unwrap();
    }
}
