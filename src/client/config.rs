//! Local client defaults. Account credentials remain in their origin-scoped store.
use crate::Result;
use serde::Deserialize;
use std::{
    ffi::OsString,
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
};
use toml_edit::{DocumentMut, Item, Table, value};

const CONFIG_LIMIT: u64 = 64 * 1024;
const HELP: &str = "\
ssmcp config set server URL    Save the default HTTPS relay
ssmcp config get server        Print the effective default relay
ssmcp config unset server      Remove the configured override
ssmcp config get git-fetch-interval       Print the background fetch interval
ssmcp config set git-fetch-interval SECS  Set 60..86400 seconds; 0 disables fetch
ssmcp config path              Print the client configuration path

File: [client] server = \"https://relay.example.com\"
Priority: --server > [client].server > last login > https://ssmcp.lmm.best
SSMCP_CLIENT_CONFIG selects another configuration file.";

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct ClientSettings {
    pub server: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct GitSettings {
    pub fetch_interval_seconds: u64,
}

impl Default for GitSettings {
    fn default() -> Self {
        Self {
            fetch_interval_seconds: 300,
        }
    }
}

impl GitSettings {
    pub(super) fn validate(&self) -> Result<()> {
        if self.fetch_interval_seconds != 0 && !(60..=86_400).contains(&self.fetch_interval_seconds)
        {
            return Err("git fetch interval must be 0 (disabled) or 60..86400 seconds".into());
        }
        Ok(())
    }
}

impl ClientSettings {
    pub(super) fn validate(&self) -> Result<()> {
        if let Some(server) = &self.server {
            super::account::normalize_server(server)?;
        }
        Ok(())
    }
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Document {
    client: ClientSettings,
    git: GitSettings,
    contexts: super::connection_context::Store,
}

pub(super) fn config_path() -> Result<PathBuf> {
    let path = if let Some(path) = std::env::var_os("SSMCP_CLIENT_CONFIG") {
        PathBuf::from(path)
    } else {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME")
                    .or_else(|| std::env::var_os("USERPROFILE"))
                    .map(|home| PathBuf::from(home).join(".config"))
            })
            .ok_or("client configuration requires HOME, USERPROFILE or XDG_CONFIG_HOME")?;
        base.join("ssmcp/config.toml")
    };
    if !path.is_absolute() {
        return Err("client configuration path must be absolute".into());
    }
    Ok(path)
}

pub(super) fn read_config(path: &Path, required: bool) -> Result<String> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if !required && error.kind() == io::ErrorKind::NotFound => {
            return Ok(String::new());
        }
        Err(_) => {
            return Err(format!("cannot read client configuration {}", path.display()).into());
        }
    };
    if !file.metadata()?.is_file() {
        return Err("client configuration must be a regular file".into());
    }
    let mut text = String::new();
    file.take(CONFIG_LIMIT + 1).read_to_string(&mut text)?;
    if text.len() as u64 > CONFIG_LIMIT {
        return Err("client configuration exceeds 64 KiB".into());
    }
    Ok(text)
}

fn parse_document(text: &str) -> Result<Document> {
    let doc: Document =
        toml::from_str(text).map_err(|_| "invalid ssmcp client TOML configuration")?;
    doc.client.validate()?;
    doc.git.validate()?;
    doc.contexts.validate()?;
    Ok(doc)
}

pub(super) fn contexts() -> Result<super::connection_context::Store> {
    let text = read_config(
        &config_path()?,
        std::env::var_os("SSMCP_CLIENT_CONFIG").is_some(),
    )?;
    Ok(parse_document(&text)?.contexts)
}

pub(super) fn git() -> Result<GitSettings> {
    let text = read_config(
        &config_path()?,
        std::env::var_os("SSMCP_CLIENT_CONFIG").is_some(),
    )?;
    Ok(parse_document(&text)?.git)
}

fn set_git_interval_at(path: &Path, seconds: u64) -> Result<()> {
    GitSettings {
        fetch_interval_seconds: seconds,
    }
    .validate()?;
    let text = read_config(path, false)?;
    let mut document: DocumentMut = text
        .parse()
        .map_err(|_| "invalid ssmcp client TOML configuration")?;
    if !document.contains_key("git") {
        document["git"] = Item::Table(Table::new());
    }
    document["git"]
        .as_table_like_mut()
        .ok_or("git configuration must be a TOML table")?
        .insert("fetch_interval_seconds", value(seconds as i64));
    save_document(path, &text, document)
}

fn parse(text: &str) -> Result<ClientSettings> {
    Ok(parse_document(text)?.client)
}

pub(super) fn server() -> Result<Option<String>> {
    let text = read_config(
        &config_path()?,
        std::env::var_os("SSMCP_CLIENT_CONFIG").is_some(),
    )?;
    parse(&text)?
        .server
        .as_deref()
        .map(super::account::normalize_server)
        .transpose()
}

fn set_server_at(path: &Path, server: Option<&str>) -> Result<()> {
    let server = server.map(super::account::normalize_server).transpose()?;
    let text = read_config(path, false)?;
    let mut document: DocumentMut = text
        .parse()
        .map_err(|_| "invalid ssmcp client TOML configuration")?;
    // Editing the TOML tree preserves UI settings, comments and other formatting.
    if let Some(server) = server {
        if !document.contains_key("client") {
            document["client"] = Item::Table(Table::new());
        }
        let client = document["client"]
            .as_table_like_mut()
            .ok_or("client configuration must be a TOML table")?;
        client.insert("server", value(server));
    } else if let Some(client) = document.get_mut("client") {
        client
            .as_table_like_mut()
            .ok_or("client configuration must be a TOML table")?
            .remove("server");
    }
    save_document(path, &text, document)
}

pub(super) fn save_document(path: &Path, text: &str, document: DocumentMut) -> Result<()> {
    let output = document.to_string();
    parse_document(&output)?;
    if output.len() as u64 > CONFIG_LIMIT {
        return Err("client configuration exceeds 64 KiB".into());
    }
    if output == text {
        return Ok(());
    }
    let parent = path
        .parent()
        .ok_or("client configuration needs a parent directory")?;
    fs::create_dir_all(parent)?;
    let mut random = [0u8; 8];
    crate::platform::random_bytes(&mut random)?;
    let suffix: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
    let temporary = path.with_extension(format!("tmp-{suffix}"));
    let result = (|| -> Result<()> {
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(output.as_bytes())?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

pub(super) fn dispatch(args: &[OsString]) -> Result<bool> {
    if !args.first().is_some_and(|arg| arg == "config") {
        return Ok(false);
    }
    let values = args[1..]
        .iter()
        .map(|arg| arg.to_str().ok_or("config arguments must be UTF-8"))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    match values.as_slice() {
        [] | ["--help"] | ["-h"] => println!("{HELP}"),
        ["path"] => println!("{}", config_path()?.display()),
        ["get", "server"] => println!("{}", super::account::resolve_server(None)?),
        ["get", "git-fetch-interval"] => println!("{}", git()?.fetch_interval_seconds),
        ["set", "git-fetch-interval", seconds] => {
            let seconds = seconds
                .parse::<u64>()
                .map_err(|_| "git fetch interval must be an integer number of seconds")?;
            let path = config_path()?;
            set_git_interval_at(&path, seconds)?;
            if seconds == 0 {
                println!("Background Git fetch: disabled");
            } else {
                println!("Background Git fetch: every {seconds} seconds");
            }
            println!(
                "Applies to new connections. Configuration: {}",
                path.display()
            );
        }
        ["set", "server", server] => {
            let server = super::account::normalize_server(server)?;
            let path = config_path()?;
            set_server_at(&path, Some(&server))?;
            println!("Default relay: {server}\nConfiguration: {}", path.display());
        }
        ["unset", "server"] => {
            let path = config_path()?;
            set_server_at(&path, None)?;
            println!(
                "Removed the configured default relay from {}",
                path.display()
            );
        }
        _ => return Err(format!("invalid config command\n{HELP}").into()),
    }
    Ok(true)
}

/// Accept a global --server before account subcommands without changing the
/// legacy SSH parser or treating a flag's value as a subcommand.
pub(super) fn normalize_args(args: &mut Vec<OsString>) -> Result<()> {
    if args.first().is_some_and(|arg| arg == "--server") {
        if args
            .get(1)
            .is_none_or(|arg| arg.to_string_lossy().starts_with('-'))
        {
            return Err("--server requires an HTTPS origin".into());
        }
        if args.get(2).and_then(|arg| arg.to_str()).is_some_and(|arg| {
            matches!(
                arg,
                "login" | "logout" | "whoami" | "list" | "ls" | "invite" | "join"
            )
        }) {
            let operation = args.remove(2);
            args.insert(0, operation);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn directory() -> PathBuf {
        let mut random = [0u8; 8];
        crate::platform::random_bytes(&mut random).unwrap();
        let root = std::env::temp_dir().join(format!(
            "ssmcp-config-{}",
            random
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        ));
        fs::create_dir(&root).unwrap();
        root
    }

    #[test]
    fn config_edits_preserve_ui_and_comments() {
        let root = directory();
        let path = root.join("config.toml");
        let original = "# workstation theme\n[ui]\ntheme = 'nord' # keep this\ncolor = false\n";
        fs::write(&path, original).unwrap();
        set_server_at(&path, Some("https://RELAY.example:443/")).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains(original));
        assert_eq!(
            parse(&text).unwrap().server.as_deref(),
            Some("https://relay.example")
        );
        assert_eq!(
            super::super::ui::Ui::parse(&text).unwrap().theme,
            super::super::ui::Theme::Nord
        );
        set_server_at(&path, None).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains(original));
        assert!(parse(&text).unwrap().server.is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn malformed_config_and_insecure_servers_are_rejected_without_changes() {
        let root = directory();
        let path = root.join("config.toml");
        fs::write(&path, "[client]\nserver = 'https://good.example'\n").unwrap();
        let before = fs::read(&path).unwrap();
        for server in [
            "http://relay.example",
            "https://user:secret@relay.example",
            "https://relay.example/path",
        ] {
            assert!(set_server_at(&path, Some(server)).is_err());
            assert_eq!(fs::read(&path).unwrap(), before);
        }
        assert!(parse("[client]\nservre='https://typo.example'").is_err());
        assert!(parse("[client]\nserver=42").is_err());
        fs::write(&path, "invalid secret-value here").unwrap();
        let error = set_server_at(&path, Some("https://good.example"))
            .unwrap_err()
            .to_string();
        assert!(!error.contains("secret-value"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn global_server_accepts_account_subcommands_and_preserves_ssh_arguments() {
        let args = |values: &[&str]| values.iter().map(OsString::from).collect::<Vec<_>>();
        let mut invite = args(&["--server", "https://relay.example", "invite", "--json"]);
        normalize_args(&mut invite).unwrap();
        assert_eq!(
            invite,
            args(&["invite", "--server", "https://relay.example", "--json"])
        );
        let mut connection = args(&["--server", "https://relay.example", "--name", "invite"]);
        let expected = connection.clone();
        normalize_args(&mut connection).unwrap();
        assert_eq!(connection, expected);
        assert!(normalize_args(&mut args(&["--server", "--json", "invite"])).is_err());
    }
    #[test]
    fn retired_browser_table_stays_inert_and_is_preserved_by_account_changes() {
        let root = directory();
        let path = root.join("config.toml");
        let legacy =
            "[browser]\nplaywright_mcp = 'legacy/cli.js'\nbackground = false\n";
        fs::write(
            &path,
            format!("{legacy}[client]\nserver = 'https://relay.example'\n"),
        )
        .unwrap();
        assert_eq!(
            parse_document(&fs::read_to_string(&path).unwrap())
                .unwrap()
                .client
                .server
                .as_deref(),
            Some("https://relay.example")
        );
        set_server_at(&path, Some("https://other.example")).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains(legacy));
        assert_eq!(
            parse_document(&text).unwrap().client.server.as_deref(),
            Some("https://other.example")
        );
        fs::remove_dir_all(root).unwrap();
    }

}

#[cfg(test)]
mod git_config_tests {
    use super::*;

    #[test]
    fn periodic_fetch_config_is_bounded_disabled_with_zero_and_preserves_other_settings() {
        assert_eq!(parse_document("").unwrap().git.fetch_interval_seconds, 300);
        for seconds in [0, 60, 300, 86_400] {
            assert_eq!(
                parse_document(&format!("[git]\nfetch_interval_seconds = {seconds}"))
                    .unwrap()
                    .git
                    .fetch_interval_seconds,
                seconds
            );
        }
        for value in ["-1", "1", "59", "86401", "'300'", "true"] {
            assert!(parse_document(&format!("[git]\nfetch_interval_seconds = {value}")).is_err());
        }
        let mut random = [0u8; 8];
        crate::platform::random_bytes(&mut random).unwrap();
        let path = std::env::temp_dir().join(format!(
            "ssmcp-fetch-config-{:x}.toml",
            u64::from_ne_bytes(random)
        ));
        let original =
            "# keep comment\n[ui]\ntheme='nord'\n[client]\nserver='https://relay.example'\n";
        fs::write(&path, original).unwrap();
        set_git_interval_at(&path, 0).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains(original));
        assert_eq!(parse_document(&text).unwrap().git.fetch_interval_seconds, 0);
        assert!(set_git_interval_at(&path, 1).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), text);
        fs::remove_file(path).unwrap();
    }
}
