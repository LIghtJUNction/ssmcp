//! Display settings are local to the client and never forwarded to a backend.
use super::render::LogFormat;
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Theme {
    Minimal,
    #[default]
    CodexDark,
    Nord,
    Solarized,
}
impl Theme {
    pub(crate) fn parse(value: &str) -> Result<Self, String> {
        match value {
            "minimal" => Ok(Self::Minimal),
            "codex-dark" => Ok(Self::CodexDark),
            "nord" => Ok(Self::Nord),
            "solarized" => Ok(Self::Solarized),
            _ => Err("theme must be minimal, codex-dark, nord, or solarized".into()),
        }
    }
    pub(crate) fn color(self, semantic: &str) -> &'static str {
        let index = match semantic {
            "red" => 0,
            "green" => 1,
            "yellow" => 2,
            "blue" => 3,
            "cyan" => 4,
            "purple" => 5,
            _ => 6,
        };
        let palette = match self {
            Self::CodexDark => ["31;1", "32;1", "33;1", "34;1", "36;1", "35;1", "90"],
            Self::Minimal => ["31", "32", "33", "34", "36", "35", "90"],
            Self::Nord => [
                "38;5;167", "38;5;150", "38;5;222", "38;5;110", "38;5;116", "38;5;139", "38;5;245",
            ],
            Self::Solarized => [
                "38;5;160", "38;5;64", "38;5;136", "38;5;33", "38;5;37", "38;5;125", "38;5;244",
            ],
        };
        palette[index]
    }
}
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Ui {
    pub color: bool,
    /// On a real terminal, isolate logs and the interactive shell with a task sidebar.
    pub dashboard: bool,
    pub log_format: LogFormat,
    pub theme: Theme,
    pub timestamps: bool,
    pub show_request_id: bool,
    pub show_raw_json: bool,
    pub max_inline_output: usize,
    /// Quiet-job status cadence; zero disables periodic status, not lifecycle events.
    pub activity_interval_secs: u64,
}
impl Default for Ui {
    fn default() -> Self {
        Self {
            color: true,
            dashboard: true,
            log_format: LogFormat::Pretty,
            theme: Theme::default(),
            timestamps: false,
            show_request_id: true,
            show_raw_json: false,
            max_inline_output: 12,
            activity_interval_secs: 5,
        }
    }
}
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Document {
    ui: Ui,
    client: super::config::ClientSettings,
    browser: super::config::BrowserSettings,
}
impl Ui {
    pub(crate) fn parse(text: &str) -> Result<Self, String> {
        // Parser diagnostics can echo accidentally pasted credentials.
        let doc: Document =
            toml::from_str(text).map_err(|_| "invalid ssmcp UI TOML configuration")?;
        doc.client.validate().map_err(|error| error.to_string())?;
        doc.browser.validate().map_err(|error| error.to_string())?;
        if !(1..=10000).contains(&doc.ui.max_inline_output) {
            return Err("ui.max_inline_output must be in 1..=10000".into());
        }
        if doc.ui.activity_interval_secs > 300 {
            return Err("ui.activity_interval_secs must be in 0..=300".into());
        }
        Ok(doc.ui)
    }
    pub(crate) fn load(explicit: Option<&Path>) -> Result<Self, String> {
        let env_path = std::env::var_os("SSMCP_UI_CONFIG").map(PathBuf::from);
        let required = explicit.is_some()
            || env_path.is_some()
            || std::env::var_os("SSMCP_CLIENT_CONFIG").is_some();
        let path = explicit
            .map(Path::to_path_buf)
            .or(env_path)
            .or_else(|| super::config::config_path().ok());
        let Some(path) = path else {
            return Ok(Self::default());
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => Self::parse(&text),
            Err(e) if !required && e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(_) => Err(format!("cannot read UI configuration {}", path.display())),
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validated_ui_config_and_themes() {
        let u=Ui::parse("[ui]\ncolor=false\nlog_format='pretty-verbose'\ntheme='nord'\nmax_inline_output=5\ntimestamps=true").unwrap();
        assert!(!u.color);
        assert!(u.dashboard);
        assert!(!Ui::parse("[ui]\ndashboard=false").unwrap().dashboard);
        assert!(u.timestamps);
        assert_eq!(u.log_format, LogFormat::PrettyVerbose);
        assert_eq!(u.theme, Theme::Nord);
        assert_eq!(u.max_inline_output, 5);
        for bad in [
            "[ui]\nmax_inline_output=0",
            "[ui]\nactivity_interval_secs=301",
            "[ui]\ntheme='unknown'",
            "[ui]\nlog_format='xml'",
            "[ui]\ncolour=true",
            "malformed secret=abc",
        ] {
            let error = Ui::parse(bad).unwrap_err();
            assert!(!error.contains("abc"));
        }
        assert!(Ui::load(Some(Path::new("/definitely/missing/ssmcp-ui.toml"))).is_err());
        assert_ne!(Theme::Nord.color("red"), Theme::Solarized.color("red"));
    }
    #[test]
    fn combined_ui_client_and_browser_config_loads_for_startup() {
        let entry = std::env::temp_dir().join("ssmcp-ui-browser-fixture.js");
        let entry = serde_json::to_string(entry.to_str().unwrap()).unwrap();
        let text = format!(
            "[ui]\ncolor = false\ntheme = 'nord'\n[client]\nserver = 'https://relay.example'\n[browser]\nplaywright_mcp = {entry}\nbackground = true\n"
        );
        let ui = Ui::parse(&text).unwrap();
        assert_eq!(ui.theme, Theme::Nord);
        assert!(!ui.color);

        let mut random = [0u8; 8];
        crate::platform::random_bytes(&mut random).unwrap();
        let suffix: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
        let path = std::env::temp_dir().join(format!("ssmcp-ui-browser-{suffix}.toml"));
        std::fs::write(&path, &text).unwrap();
        let loaded = Ui::load(Some(&path));
        std::fs::remove_file(path).unwrap();
        let loaded = loaded.unwrap();
        assert_eq!(loaded.theme, Theme::Nord);
        assert!(!loaded.color);
    }

    #[test]
    fn browser_settings_keep_their_validation_in_ui_configuration() {
        assert!(
            !Ui::parse("[browser]\nbackground = false\n")
                .unwrap()
                .timestamps
        );
        for text in [
            "[browser]\nplaywright_mcp = 'relative/cli.js'\n",
            "[browser]\nbackground = 'secret-value'\n",
            "[browser]\nplaywrigt_mcp = '/tmp/secret-value'\n",
        ] {
            let error = Ui::parse(text).unwrap_err();
            assert!(!error.contains("secret-value"), "{error}");
        }
    }
}
