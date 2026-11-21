//! Display settings are local to the client and never forwarded to a backend.
use super::render::LogFormat;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub(crate) mod pet;

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Theme {
    #[default]
    Dark,
    Minimal,
    CodexDark,
    Nord,
    Solarized,
    Light,
}

impl Theme {
    pub(crate) fn parse(value: &str) -> Result<Self, String> {
        match value {
            "dark" => Ok(Self::Dark),
            "minimal" => Ok(Self::Minimal),
            "codex-dark" => Ok(Self::CodexDark),
            "nord" => Ok(Self::Nord),
            "solarized" => Ok(Self::Solarized),
            "light" => Ok(Self::Light),
            _ => Err("theme must be dark, minimal, codex-dark, nord, solarized, or light".into()),
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Dark => "dark",
            Self::Minimal => "minimal",
            Self::CodexDark => "codex-dark",
            Self::Nord => "nord",
            Self::Solarized => "solarized",
            Self::Light => "light",
        }
    }

    pub(crate) fn next(self) -> Self {
        match self {
            Self::Dark => Self::CodexDark,
            Self::CodexDark => Self::Nord,
            Self::Nord => Self::Solarized,
            Self::Solarized => Self::Light,
            Self::Light => Self::Minimal,
            Self::Minimal => Self::Dark,
        }
    }

    pub(crate) fn color(self, semantic: &str) -> &'static str {
        let index = match semantic {
            "red" | "error" => 0,
            "green" | "success" => 1,
            "yellow" | "warning" => 2,
            "blue" => 3,
            "cyan" | "accent" => 4,
            "purple" => 5,
            "foreground" | "normal" => 7,
            "background" => 8,
            "heading" => 9,
            _ => 6,
        };
        let palette = match self {
            Self::Dark => [
                "38;2;242;139;130",
                "38;2;129;201;149",
                "38;2;253;214;99",
                "38;2;138;180;248",
                "38;2;120;217;236",
                "38;2;197;138;249",
                "38;2;154;160;166",
                "38;2;232;234;237",
                "48;2;24;24;22",
                "1;38;2;232;234;237",
            ],
            Self::Minimal => ["31", "32", "33", "34", "36", "35", "90", "39", "49", "1;39"],
            Self::CodexDark => [
                "38;2;242;139;130",
                "38;2;129;201;149",
                "38;2;253;214;99",
                "38;2;138;180;248",
                "38;2;138;180;248",
                "38;2;197;138;249",
                "38;2;154;160;166",
                "38;2;232;234;237",
                "48;2;16;18;20",
                "1;38;2;232;234;237",
            ],
            Self::Nord => [
                "38;2;191;97;106",
                "38;2;163;190;140",
                "38;2;235;203;139",
                "38;2;129;161;193",
                "38;2;136;192;208",
                "38;2;180;142;173",
                "38;2;166;174;190",
                "38;2;236;239;244",
                "48;2;46;52;64",
                "1;38;2;236;239;244",
            ],
            Self::Solarized => [
                "38;2;220;50;47",
                "38;2;133;153;0",
                "38;2;181;137;0",
                "38;2;38;139;210",
                "38;2;42;161;152",
                "38;2;211;54;130",
                "38;2;131;148;150",
                "38;2;147;161;161",
                "48;2;0;43;54",
                "1;38;2;238;232;213",
            ],
            Self::Light => [
                "38;2;179;38;30",
                "38;2;19;115;51",
                "38;2;146;89;0",
                "38;2;25;103;210",
                "38;2;25;103;210",
                "38;2;132;48;206",
                "38;2;95;99;104",
                "38;2;32;33;36",
                "48;2;255;255;255",
                "1;38;2;32;33;36",
            ],
        };
        palette[index]
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Layout {
    #[default]
    Chat,
    Split,
    Audit,
    Logs,
}

impl Layout {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Split => "split",
            Self::Audit => "audit",
            Self::Logs => "logs",
        }
    }

    pub(crate) fn next(self) -> Self {
        match self {
            Self::Chat => Self::Split,
            Self::Split => Self::Audit,
            Self::Audit => Self::Logs,
            Self::Logs => Self::Chat,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Panel {
    Agents,
    Queue,
    Controller,
    Tasks,
    Progress,
    Git,
    History,
}

/// Empty colors inherit the selected theme. Explicit "default" follows the
/// terminal's own foreground/background instead of the selected theme.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Colors {
    #[serde(skip_serializing_if = "String::is_empty")]
    pub background: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub foreground: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub muted: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub accent: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub success: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub warning: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub error: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub heading: String,
}

/// Produce an SGR fragment from a restricted color value; never emit user text.
fn color_code(value: &str, background: bool) -> Option<String> {
    let base = if background { 48 } else { 38 };
    if value == "default" {
        return Some(if background { "49" } else { "39" }.into());
    }
    if let Some(hex) = value.strip_prefix('#') {
        if hex.len() != 6 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
        let rgb = u32::from_str_radix(hex, 16).ok()?;
        return Some(format!(
            "{base};2;{};{};{}",
            (rgb >> 16) & 255,
            (rgb >> 8) & 255,
            rgb & 255
        ));
    }
    let (bright, name) = value
        .strip_prefix("bright-")
        .map_or((false, value), |name| (true, name));
    let index = match name {
        "black" => 0,
        "red" => 1,
        "green" => 2,
        "yellow" => 3,
        "blue" => 4,
        "magenta" | "purple" => 5,
        "cyan" => 6,
        "white" => 7,
        "gray" | "grey" if !bright => return Some(if background { "100" } else { "90" }.into()),
        _ => return None,
    };
    Some((index + if background { 40 } else { 30 } + if bright { 60 } else { 0 }).to_string())
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Ui {
    pub color: bool,
    /// On a real terminal, show logs, source audit and the configured sidebar.
    pub dashboard: bool,
    pub log_format: LogFormat,
    pub theme: Theme,
    pub colors: Colors,
    pub pet: pet::Pet,
    pub layout: Layout,
    pub log_height_percent: u16,
    pub sidebar: bool,
    pub sidebar_width: u16,
    pub panels: Vec<Panel>,
    pub hide_empty: bool,
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
            colors: Colors::default(),
            pet: pet::Pet::default(),
            layout: Layout::default(),
            log_height_percent: 60,
            sidebar: false,
            sidebar_width: 37,
            panels: vec![
                Panel::Agents,
                Panel::Queue,
                Panel::Controller,
                Panel::Tasks,
                Panel::Progress,
                Panel::Git,
                Panel::History,
            ],
            hide_empty: true,
            timestamps: false,
            show_request_id: true,
            show_raw_json: false,
            max_inline_output: 8,
            activity_interval_secs: 5,
        }
    }
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Document {
    ui: Ui,
    client: super::config::ClientSettings,
    // Accept saved settings from older clients without configuring a controller.
    #[serde(rename = "browser")]
    _legacy_browser: Option<serde::de::IgnoredAny>,
    git: super::config::GitSettings,
    contexts: super::connection_context::Store,
}

impl Ui {
    pub(crate) fn parse(text: &str) -> Result<Self, String> {
        // Parser diagnostics can echo accidentally pasted credentials.
        let doc: Document =
            toml::from_str(text).map_err(|_| "invalid ssmcp UI TOML configuration")?;
        doc.client.validate().map_err(|error| error.to_string())?;
        doc.git.validate().map_err(|error| error.to_string())?;
        doc.contexts.validate().map_err(|error| error.to_string())?;
        doc.ui.validate()?;
        Ok(doc.ui)
    }

    pub(crate) fn validate(&self) -> Result<(), String> {
        self.pet.validate()?;
        if !(1..=10000).contains(&self.max_inline_output) {
            return Err("ui.max_inline_output must be in 1..=10000".into());
        }
        if self.activity_interval_secs > 300 {
            return Err("ui.activity_interval_secs must be in 0..=300".into());
        }
        if !(10..=80).contains(&self.log_height_percent) {
            return Err("ui.log_height_percent must be in 10..=80".into());
        }
        if !(20..=60).contains(&self.sidebar_width) {
            return Err("ui.sidebar_width must be in 20..=60".into());
        }
        if self
            .panels
            .iter()
            .enumerate()
            .any(|(index, panel)| self.panels[..index].contains(panel))
        {
            return Err("ui.panels must not contain duplicates".into());
        }
        for (name, value) in [
            ("background", &self.colors.background),
            ("foreground", &self.colors.foreground),
            ("muted", &self.colors.muted),
            ("accent", &self.colors.accent),
            ("success", &self.colors.success),
            ("warning", &self.colors.warning),
            ("error", &self.colors.error),
            ("heading", &self.colors.heading),
        ] {
            if !value.is_empty() && color_code(value, name == "background").is_none() {
                return Err(format!(
                    "ui.colors.{name} must be #RRGGBB, an ANSI color name, or default"
                ));
            }
        }
        Ok(())
    }

    pub(crate) fn color(&self, semantic: &str) -> String {
        let value = match semantic {
            "foreground" | "normal" => &self.colors.foreground,
            "background" => return self.background(),
            "gray" | "grey" | "muted" => &self.colors.muted,
            "blue" | "cyan" | "purple" | "accent" => &self.colors.accent,
            "green" | "success" => &self.colors.success,
            "yellow" | "warning" => &self.colors.warning,
            "red" | "error" => &self.colors.error,
            "heading" => &self.colors.heading,
            _ => &self.colors.foreground,
        };
        if !value.is_empty() {
            // Invalid programmatic values are also kept out of terminal output.
            return color_code(value, false).unwrap_or_else(|| "39".into());
        }
        if semantic == "heading" && !self.colors.foreground.is_empty() {
            return format!("1;{}", self.color("foreground"));
        }
        self.theme.color(semantic).into()
    }

    pub(crate) fn background(&self) -> String {
        if self.colors.background.is_empty() {
            self.theme.color("background").into()
        } else {
            color_code(&self.colors.background, true).unwrap_or_else(|| "49".into())
        }
    }

    /// A quiet input surface; explicit background overrides remain authoritative.
    pub(crate) fn input_background(&self) -> String {
        if !self.colors.background.is_empty() {
            return self.background();
        }
        match self.theme {
            Theme::Dark => "48;2;44;46;50",
            Theme::CodexDark => "48;2;41;44;50",
            Theme::Nord => "48;2;59;66;82",
            Theme::Solarized => "48;2;7;54;66",
            Theme::Light => "48;2;241;243;244",
            Theme::Minimal => return self.background(),
        }
        .into()
    }

    pub(crate) fn to_toml(&self) -> Result<String, String> {
        self.validate()?;
        #[derive(Serialize)]
        struct UiDocument<'a> {
            ui: &'a Ui,
        }
        toml::to_string_pretty(&UiDocument { ui: self })
            .map_err(|_| "cannot serialize UI configuration".into())
    }

    /// Merge a partial UI object. Colors merge by key; panels replace the list.
    /// Validation happens before the caller can replace its current settings.
    pub(crate) fn merge_json(&self, patch: serde_json::Value) -> Result<Self, String> {
        fn merge(target: &mut serde_json::Value, patch: serde_json::Value) {
            match (target, patch) {
                (serde_json::Value::Object(target), serde_json::Value::Object(patch)) => {
                    for (key, value) in patch {
                        merge(target.entry(key).or_insert(serde_json::Value::Null), value);
                    }
                }
                (target, value) => *target = value,
            }
        }
        if !patch.is_object() {
            return Err("UI patch must be an object".into());
        }
        let mut value =
            serde_json::to_value(self).map_err(|_| "cannot serialize UI configuration")?;
        merge(&mut value, patch);
        let updated: Self =
            serde_json::from_value(value).map_err(|_| "invalid UI configuration patch")?;
        updated.validate()?;
        Ok(updated)
    }

    pub(crate) fn config_path(explicit: Option<&Path>) -> Option<PathBuf> {
        explicit
            .map(Path::to_path_buf)
            .or_else(|| std::env::var_os("SSMCP_UI_CONFIG").map(PathBuf::from))
            .or_else(|| super::config::config_path().ok())
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
    use serde_json::json;

    #[test]
    fn pet_configuration_roundtrips_and_partial_updates_preserve_custom_text() {
        use super::pet::Preset;
        assert_eq!(Ui::parse("").unwrap().pet.preset, Preset::Cat);
        let custom =
            Ui::parse("[ui.pet]\npreset = 'custom'\ntext = '''\n /\\_/\\\n( o.o )\n > ^ <\n'''\n")
                .unwrap();
        assert_eq!(custom.pet.preset, Preset::Custom);
        assert_eq!(custom.pet.lines(), [" /\\_/\\", "( o.o )", " > ^ <"]);
        assert_eq!(Ui::parse(&custom.to_toml().unwrap()).unwrap(), custom);
        let off = custom.merge_json(json!({"pet":{"preset":"off"}})).unwrap();
        assert_eq!(off.pet.preset, Preset::Off);
        assert_eq!(off.pet.text, custom.pet.text);
        let robot = custom
            .merge_json(json!({"pet":{"preset":"robot"}}))
            .unwrap();
        assert_eq!(robot.pet.preset, Preset::Robot);
        for patch in [
            json!({"pet":null}),
            json!({"pet":{"preset":"unknown"}}),
            json!({"pet":{"preset":"custom","text":""}}),
            json!({"pet":{"preset":"custom","text":"\u{1b}[2J"}}),
            json!({"pet":{"command":"run something"}}),
        ] {
            assert!(custom.merge_json(patch).is_err());
        }
    }

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
        assert_eq!(u.log_height_percent, 60);
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
        let mut theme = Theme::default();
        for _ in 0..6 {
            assert_eq!(Theme::parse(theme.name()).unwrap(), theme);
            theme = theme.next();
        }
        assert_eq!(theme, Theme::default());
    }

    #[test]
    fn custom_colors_and_layout_roundtrip_without_raw_escape_sequences() {
        let u = Ui::parse(
            "[ui]\nlayout='audit'\nsidebar_width=24\npanels=['git','tasks']\n\
             [ui.colors]\nbackground='#101216'\nforeground='#EAEEF3'\naccent='#8ab4f8'\nwarning='bright-yellow'",
        ).unwrap();
        assert_eq!(u.layout, Layout::Audit);
        assert_eq!(u.panels, vec![Panel::Git, Panel::Tasks]);
        assert_eq!(u.color("cyan"), "38;2;138;180;248");
        assert_eq!(u.color("foreground"), "38;2;234;238;243");
        assert_eq!(u.background(), "48;2;16;18;22");
        assert_eq!(u.color("yellow"), "93");
        assert_eq!(u.color("red"), u.theme.color("red"));
        assert_eq!(Ui::parse(&u.to_toml().unwrap()).unwrap(), u);
        let terminal = Ui::default()
            .merge_json(json!({"colors":{"background":"default","foreground":"default"}}))
            .unwrap();
        assert_eq!(terminal.background(), "49");
        assert_eq!(terminal.color("foreground"), "39");
        assert_eq!(Ui::default().background(), "48;2;24;24;22");
    }

    #[test]
    fn partial_patch_preserves_other_colors_and_rejects_invalid_data() {
        let old = Ui::default()
            .merge_json(json!({"colors":{"accent":"#123456"},"sidebar":true}))
            .unwrap();
        let new = old
            .merge_json(json!({"colors":{"background":"black"},"sidebar":false}))
            .unwrap();
        assert_eq!(new.colors.accent, "#123456");
        assert_eq!(new.background(), "40");
        assert!(!new.sidebar);
        assert!(old.sidebar);
        for patch in [
            json!([]),
            json!({"colors":{"accent":"\u{1b}[2Jabc"}}),
            json!({"colors":{"accent":"#12345"}}),
            json!({"colors":{"accent":"#12GG34"}}),
            json!({"colors":{"accent":null}}),
            json!({"colors":{"unknown":"abc"}}),
            json!({"log_height_percent":9}),
            json!({"log_height_percent":81}),
            json!({"sidebar_width":19}),
            json!({"sidebar_width":61}),
            json!({"layout":"abc"}),
            json!({"panels":["git","git"]}),
            json!({"panels":["abc"]}),
            json!({"unknown":"abc"}),
        ] {
            let error = old.merge_json(patch).unwrap_err();
            assert!(!error.contains("abc"));
        }
        assert!(old.merge_json(json!({"panels":[]})).is_ok());
        let reset = old.merge_json(json!({"colors":{"accent":""}})).unwrap();
        assert_eq!(reset.color("accent"), reset.theme.color("accent"));
        assert_eq!(old.colors.accent, "#123456");
    }
    #[test]
    fn combined_ui_client_legacy_browser_and_git_config_loads_for_startup() {
        let entry = std::env::temp_dir().join("ssmcp-ui-browser-fixture.js");
        let entry = serde_json::to_string(entry.to_str().unwrap()).unwrap();
        let text = format!(
            "[ui]\ncolor = false\ntheme = 'nord'\n[client]\nserver = 'https://relay.example'\n[browser]\nplaywright_mcp = {entry}\nbackground = true\n[git]\nfetch_interval_seconds = 300\n"
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
    fn connection_contexts_keep_their_validation_in_ui_configuration() {
        for current in ["here", "removed"] {
            let text = format!(
                "[ui]\ncolor = false\ntheme = 'nord'\n\
                 [contexts]\ncurrent = '{current}'\n\
                 [contexts.entries.here]\nendpoint = 'local'\n\
                 [contexts.entries.work]\nendpoint = 'https://relay.example'\n"
            );
            let ui = Ui::parse(&text).unwrap();
            assert_eq!(ui.theme, Theme::Nord);
            assert!(!ui.color);
        }
        // A stale current selection can be overridden explicitly, but malformed
        // entries and unknown fields must still fail before startup.
        for text in [
            "[contexts.entries.work]\nendpoint = 'ssh://secret-value'\n",
            "[contexts.entries.work]\nendpoint = 'http://secret-value'\n",
            "[contexts.entries.work]\nendpoint = 'local'\ntoken = 'secret-value'\n",
            "[contexts]\ncurrent = 'secret-value with spaces'\n",
            "[contexts]\nunknown = 'secret-value'\n",
            "[contextz]\ncurrent = 'secret-value'\n",
        ] {
            let error = Ui::parse(text).unwrap_err();
            assert!(!error.contains("secret-value"), "{error}");
        }
    }

    #[test]
    fn git_settings_keep_their_validation_in_ui_configuration() {
        for seconds in [0, 60, 300, 86_400] {
            let text =
                format!("[ui]\nlog_format = 'jsonl'\n[git]\nfetch_interval_seconds = {seconds}\n");
            assert_eq!(Ui::parse(&text).unwrap().log_format, LogFormat::Jsonl);
        }
        for value in ["1", "59", "86401", "'secret-value'", "true"] {
            let text = format!("[git]\nfetch_interval_seconds = {value}\n");
            let error = Ui::parse(&text).unwrap_err();
            assert!(!error.contains("secret-value"), "{error}");
        }
        assert!(Ui::parse("[git]\nfetch_intervla = 'secret-value'\n").is_err());
        Ui::parse(include_str!("../../packaging/ssmcp-client.toml.example")).unwrap();
    }

    #[test]
    fn retired_browser_settings_are_ignored_without_weakening_ui_validation() {
        let ui = Ui::parse(
            "[ui]\ntheme = 'nord'\n[browser]\nplaywright_mcp = 'legacy/cli.js'\nbackground = false\n",
        )
        .unwrap();
        assert_eq!(ui.theme, Theme::Nord);
        assert!(!ui.timestamps);
        for text in [
            "[ui]\ntheme = 'secret-value'\n[browser]\nbackground = false\n",
            "[unknown]\nvalue = 'secret-value'\n",
        ] {
            let error = Ui::parse(text).unwrap_err();
            assert!(!error.contains("secret-value"), "{error}");
        }
    }
}
