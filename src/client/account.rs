//! Relay account discovery and device registration.
use crate::{Result, platform};
use reqwest::{Client, Method, StatusCode, Url};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
#[cfg(any(test, target_os = "linux"))]
use sha2::{Digest, Sha256};
use std::{
    ffi::OsString,
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::process::Command;

pub(crate) mod device_identity;
pub(crate) mod foreground_identity;
pub(super) mod status;

pub(super) const DEFAULT_SERVER: &str = "https://ssmcp.lmm.best";
const STORE_LIMIT: usize = 32 * 1024;
const RESPONSE_LIMIT: usize = 8 * 1024 * 1024;
#[cfg(any(test, target_os = "linux"))]
const MAX_USERNAME_ALIASES: usize = 128;

#[cfg(any(test, target_os = "linux"))]
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Credential {
    pub server: String,
    pub token: String,
    pub principal: String,
    #[serde(default)]
    pub subject: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
    pub expires_at: u64,
}

#[cfg(any(test, target_os = "linux"))]
impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credential")
            .field("server", &self.server)
            .field("principal", &self.principal)
            .field("username", &self.username)
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}

pub(super) use crate::server_endpoint::{normalize_server, parse_target};

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(any(test, target_os = "linux"))]
fn valid_secret(value: &str) -> bool {
    (32..=1024).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

#[cfg(any(test, target_os = "linux"))]
fn valid_credential(value: &Credential) -> Result<()> {
    if normalize_server(&value.server)? != value.server
        || crate::wire::canonical_github_principal(&value.principal)? != value.principal
        || !valid_secret(&value.token)
        || value
            .subject
            .as_ref()
            .is_some_and(|v| v.is_empty() || v.len() > 64 || !v.bytes().all(|b| b.is_ascii_digit()))
        || value
            .username
            .as_ref()
            .is_some_and(|name| crate::username::normalize(name).as_deref() != Ok(name.as_str()))
        || value.expires_at == 0
    {
        return Err(
            "invalid saved account credential; authorize your controller through MCP OAuth".into(),
        );
    }
    Ok(())
}

pub(super) fn state_root() -> Result<PathBuf> {
    let root = if let Some(path) = std::env::var_os("SSMCP_AUTH_HOME") {
        PathBuf::from(path)
    } else {
        #[cfg(windows)]
        {
            PathBuf::from(std::env::var_os("LOCALAPPDATA").ok_or("LOCALAPPDATA is unavailable")?)
                .join("ssmcp/auth")
        }
        #[cfg(not(windows))]
        {
            let base = match std::env::var_os("XDG_CONFIG_HOME") {
                Some(path) => PathBuf::from(path),
                None => PathBuf::from(std::env::var_os("HOME").ok_or("HOME is unavailable")?)
                    .join(".config"),
            };
            base.join("ssmcp/auth")
        }
    };
    if !root.is_absolute() {
        return Err("account credential directory must be absolute".into());
    }
    Ok(root)
}

pub(super) fn create_tree(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => return Ok(()),
        Ok(_) => return Err("account state path must be a regular directory".into()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    create_tree(path.parent().ok_or("account state path has no parent")?)?;
    match platform::private_directory(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => create_tree(path),
        Err(error) => Err(error.into()),
    }
}

pub(super) fn verify_directory(path: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_dir() || meta.file_type().is_symlink() {
        return Err("account credential directory must not be a symlink".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if meta.uid() != unsafe { libc::geteuid() } || meta.permissions().mode() & 0o077 != 0 {
            return Err(
                "account credential directory must belong to this user with mode 0700".into(),
            );
        }
    }
    #[cfg(windows)]
    drop(platform::windows::private_directory(path)?);
    Ok(())
}

pub(super) fn read_private(path: &Path) -> Result<Option<Vec<u8>>> {
    read_private_limit(path, STORE_LIMIT)
}

pub(super) fn read_private_limit(path: &Path, limit: usize) -> Result<Option<Vec<u8>>> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let meta = file.metadata()?;
    if !meta.is_file() || fs::symlink_metadata(path)?.file_type().is_symlink() {
        return Err("account credential must be a regular file".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if meta.uid() != unsafe { libc::geteuid() } || meta.permissions().mode() & 0o077 != 0 {
            return Err("account credential must belong to this user with mode 0600".into());
        }
    }
    #[cfg(windows)]
    platform::windows::verify_file(&file)?;
    if meta.len() > limit as u64 {
        return Err("account credential exceeds its size limit".into());
    }
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err("account credential exceeds its size limit".into());
    }
    Ok(Some(bytes))
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    write_private_limit(path, bytes, STORE_LIMIT)
}

pub(super) fn write_private_limit(path: &Path, bytes: &[u8], limit: usize) -> Result<()> {
    if bytes.len() > limit {
        return Err("account credential exceeds its size limit".into());
    }
    let mut random = [0; 16];
    platform::random_bytes(&mut random)?;
    let suffix: String = crate::hex::encode(random);
    let temporary = path.with_extension(format!("tmp-{suffix}"));
    let result = (|| -> Result<()> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let mut file = options.open(&temporary)?;
        #[cfg(windows)]
        platform::windows::verify_file(&file)?;
        file.write_all(bytes)?;
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

#[derive(Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub(super) enum LastConnection {
    Local {
        #[serde(default)]
        listen: Option<String>,
        #[serde(default)]
        public_url: Option<String>,
        #[serde(default)]
        socket: Option<PathBuf>,
    },
    Relay {
        username: String,
        server: String,
    },
}

pub(super) fn last_connection() -> Result<Option<LastConnection>> {
    let root = state_root()?;
    if !root.try_exists()? {
        return Ok(None);
    }
    verify_directory(&root)?;
    read_private(&root.join("last-connection.json"))?
        .map(|bytes| serde_json::from_slice(&bytes).map_err(Into::into))
        .transpose()
}

pub(super) fn remember_connection(target: &LastConnection) -> Result<()> {
    let root = state_root()?;
    create_tree(&root)?;
    verify_directory(&root)?;
    write_private(
        &root.join("last-connection.json"),
        &serde_json::to_vec(target)?,
    )
}

pub(super) fn connection_username(server: &str) -> Result<String> {
    if let Some(LastConnection::Relay {
        username,
        server: previous,
    }) = last_connection()?
        && previous == server
    {
        return Ok(username);
    }
    Err("choose an account with ssmcp username@server, or run ssmcp local".into())
}

#[cfg(any(test, target_os = "linux"))]
fn credential_path(root: &Path, server: &str) -> PathBuf {
    root.join(format!(
        "{}.json",
        crate::hex::encode(Sha256::digest(server.as_bytes()))
    ))
}

fn selected_server(root: &Path, explicit: Option<&str>) -> Result<String> {
    if let Some(server) = explicit {
        return normalize_server(server);
    }
    if let Some(bytes) = read_private(&root.join("default"))? {
        return normalize_server(std::str::from_utf8(&bytes)?.trim());
    }
    normalize_server(DEFAULT_SERVER)
}

#[cfg(any(test, target_os = "linux"))]
fn load_at(root: &Path, server: Option<&str>, include_expired: bool) -> Result<Option<Credential>> {
    match verify_directory(root) {
        Ok(()) => {}
        Err(error)
            if error
                .downcast_ref::<io::Error>()
                .is_some_and(|e| e.kind() == io::ErrorKind::NotFound) =>
        {
            return Ok(None);
        }
        Err(error) => return Err(error),
    }
    let server = selected_server(root, server)?;
    let Some(bytes) = read_private(&credential_path(root, &server))? else {
        return Ok(None);
    };
    let value: Credential = serde_json::from_slice(&bytes)?;
    valid_credential(&value)?;
    if value.server != server {
        return Err("saved credential belongs to another server".into());
    }
    if !include_expired && value.expires_at <= now() {
        return Err(
            "account login has expired; authorize your controller through MCP OAuth".into(),
        );
    }
    Ok(Some(value))
}

#[cfg(test)]
fn save_at(root: &Path, credential: &Credential) -> Result<()> {
    valid_credential(credential)?;
    create_tree(root)?;
    verify_directory(root)?;
    write_private(
        &credential_path(root, &credential.server),
        &serde_json::to_vec(credential)?,
    )?;
    write_private(&root.join("default"), credential.server.as_bytes())?;
    Ok(())
}

/// One selector for login, account commands, normal controlled connections.
pub(super) fn resolve_server(explicit: Option<&str>) -> Result<String> {
    if let Some(server) = explicit {
        return normalize_server(server);
    }
    if let Some(server) = super::config::server()? {
        return Ok(server);
    }
    let root = state_root()?;
    if root.exists() {
        verify_directory(&root)?;
    }
    selected_server(&root, None)
}

/// Resolve only the selected origin; never borrow another server's credential.
#[cfg(target_os = "linux")]
pub(super) async fn credentials(server: Option<&str>) -> Result<Option<Credential>> {
    let server = resolve_server(server)?;
    load_at(&state_root()?, Some(&server), false)
}

#[cfg(target_os = "linux")]
pub(super) fn recovery_credential_stamp(server: &str) -> Result<Option<(String, String)>> {
    let server = normalize_server(server)?;
    Ok(
        load_at(&state_root()?, Some(&server), true)?.map(|credential| {
            (
                credential.subject.unwrap_or(credential.principal),
                crate::wire::device_address::credential_key(&credential.token),
            )
        }),
    )
}

#[derive(Clone)]
struct Api {
    client: Client,
    server: String,
}
struct Reply {
    status: StatusCode,
    date: Option<std::time::SystemTime>,
    value: Value,
}

impl Api {
    fn new(server: &str) -> Result<Self> {
        let server = normalize_server(server)?;
        let mut builder = crate::http_client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(15))
            .user_agent(concat!("ssmcp/", env!("CARGO_PKG_VERSION"), " client"));
        if let Some(tls) = super::trust::tls_config(&server)? {
            builder = builder.tls_backend_preconfigured((*tls).clone());
        }
        Ok(Self {
            client: builder.build()?,
            server,
        })
    }
    async fn request(
        &self,
        method: Method,
        route: &str,
        token: Option<&str>,
        body: Option<Value>,
    ) -> Result<Reply> {
        let mut request = self
            .client
            .request(method, format!("{}{route}", self.server));
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        let mut response = request
            .send()
            .await
            .map_err(|_| "server request failed or timed out; check connectivity, the TLS certificate and trusted server public keys")?;
        let status = response.status();
        if status.is_redirection() {
            return Err("server API redirected the request; credentials were not forwarded".into());
        }
        let date = response
            .headers()
            .get(reqwest::header::DATE)
            .and_then(|value| httpdate::parse_http_date(value.to_str().ok()?).ok());
        if response
            .content_length()
            .is_some_and(|n| n > RESPONSE_LIMIT as u64)
        {
            return Err("server response exceeds its size limit".into());
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| "server response was interrupted")?
        {
            if bytes.len().saturating_add(chunk.len()) > RESPONSE_LIMIT {
                return Err("server response exceeds its size limit".into());
            }
            bytes.extend_from_slice(&chunk);
        }
        let value = if bytes.is_empty() && status.is_success() {
            json!({})
        } else {
            serde_json::from_slice(&bytes).map_err(|_| {
                if status == StatusCode::NOT_FOUND {
                    "this server does not provide client account APIs; update ssmcpd"
                } else {
                    "server returned invalid JSON"
                }
            })?
        };
        Ok(Reply {
            status,
            date,
            value,
        })
    }
    fn checked(reply: Reply) -> Result<Value> {
        if !reply.status.is_success() {
            // Match only known codes: arbitrary error bodies may reflect request secrets.
            let message = match reply.value["error"].as_str() {
                Some("io_reply_unsupported") => Some(
                    "selected client has not advertised ssmcp.ui.reply; update that client to use chat I/O, or use explicit --terminal for its existing PTY",
                ),
                Some("io_capability_unavailable") => Some(
                    "could not verify the selected client's chat reply capability; no chat input was queued; retry when the client is available",
                ),
                Some("io_request_conflict") => Some(
                    "request_id already identifies different text or a different fixed recipient set; inspect its receipt before retrying",
                ),
                Some("io_unavailable") => Some(
                    "chat request or recipient is unavailable for this account, connection, or retention window",
                ),
                Some("io_capacity") => Some(
                    "chat request/output retention is full; existing request IDs remain readable and retryable",
                ),
                Some("io_invalid_request") => {
                    Some("invalid chat input, target, output cursor or reply state")
                }
                Some("account_not_found") => {
                    Some("this username is unavailable on the selected server")
                }
                Some("account_banned") => {
                    Some("this account is banned on this server; contact its administrator")
                }
                Some("username_taken") => {
                    Some("this username is already registered to another GitHub account")
                }
                Some("username_mismatch" | "username_bound") => {
                    Some("this GitHub account is already registered with a different username")
                }
                Some("username_invalid" | "invalid_username") => Some(
                    "choose a valid, unreserved username: 3–32 ASCII letters, digits, underscores or hyphens; begin with a letter and end with a letter or digit",
                ),
                _ => None,
            };
            if let Some(message) = message {
                return Err(message.into());
            }
            if reply.status == StatusCode::UNAUTHORIZED {
                return Err(
                    "account authorization expired; reconnect with ssmcp username@server".into(),
                );
            }
            if reply.status == StatusCode::FORBIDDEN {
                return Err(
                    "account access is disabled on this server; contact its administrator".into(),
                );
            }
            return Err(format!("server rejected the request ({})", reply.status).into());
        }
        Ok(reply.value)
    }
    #[cfg(any(test, target_os = "linux"))]
    async fn get(&self, route: &str, token: &str) -> Result<Value> {
        Self::checked(self.request(Method::GET, route, Some(token), None).await?)
    }
}

#[derive(Default)]
struct Arguments {
    server: Option<String>,
    context: Option<String>,
    json: bool,
    help: bool,
    no_browser: bool,
    name: Option<String>,
}
impl Arguments {
    fn parse(args: &[OsString]) -> Result<Self> {
        let mut result = Self::default();
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            let mut value = || -> Result<String> {
                Ok(args
                    .next()
                    .ok_or("option requires a value")?
                    .to_str()
                    .ok_or("argument must be UTF-8")?
                    .to_owned())
            };
            match arg.to_str() {
                Some("--server") => {
                    if result.server.is_some() {
                        return Err("--server may be supplied only once".into());
                    }
                    result.server = Some(normalize_server(&value()?)?);
                }
                Some("--context") => {
                    super::connection_context::set_name(&mut result.context, &value()?)?
                }
                Some(value) if value.starts_with("--context=") => {
                    super::connection_context::set_name(
                        &mut result.context,
                        &value["--context=".len()..],
                    )?;
                }
                Some("--json") => result.json = true,
                Some("--help" | "-h") => result.help = true,
                Some("--no-browser") => result.no_browser = true,
                Some("--name") => result.name = Some(value()?),
                _ => return Err("unknown account option; see ssmcp whoami --help".into()),
            }
        }
        Ok(result)
    }
}

#[cfg(any(test, target_os = "linux"))]
fn public_identity(value: &Value) -> Result<Value> {
    let identity = value.get("identity").unwrap_or(value);
    let login = identity["login"]
        .as_str()
        .or_else(|| identity["github_login"].as_str())
        .ok_or("server omitted the GitHub login")?;
    let principal = crate::wire::canonical_github_principal(&format!("{login}@github"))?;
    if identity
        .get("principal")
        .is_some_and(|v| v.as_str() != Some(&principal))
    {
        return Err("server returned an inconsistent GitHub principal".into());
    }
    if identity.get("provider").is_some_and(|v| v != "github") {
        return Err("server returned an unsupported identity provider".into());
    }
    let subject = identity["subject"]
        .as_str()
        .or_else(|| identity["github_id"].as_str())
        .filter(|s| !s.is_empty() && s.len() <= 64 && s.bytes().all(|b| b.is_ascii_digit()))
        .ok_or("server omitted the stable GitHub subject")?;
    let mut public =
        json!({"provider":"github","subject":subject,"login":login,"principal":principal});
    if let Some(value) = identity.get("username").filter(|value| !value.is_null()) {
        let username = value
            .as_str()
            .ok_or("server returned an invalid username")?;
        if crate::username::normalize(username).as_deref() != Ok(username) {
            return Err("server returned an invalid username".into());
        }
        public["username"] = json!(username);
    }
    if let Some(value) = identity.get("username_aliases") {
        let aliases = value
            .as_array()
            .filter(|aliases| aliases.len() <= MAX_USERNAME_ALIASES)
            .ok_or("server returned invalid username aliases")?;
        if aliases.iter().any(|value| {
            value
                .as_str()
                .is_none_or(|alias| crate::username::normalize(alias).as_deref() != Ok(alias))
        }) || (!aliases.is_empty() && public["username"].is_null())
        {
            return Err("server returned invalid username aliases".into());
        }
        public["username_aliases"] = value.clone();
    }
    if let Some(value) = identity.get("username_generated") {
        let generated = value
            .as_bool()
            .ok_or("server returned invalid username generation status")?;
        if generated && public["username"].is_null() {
            return Err("server returned invalid username generation status".into());
        }
        public["username_generated"] = json!(generated);
    }
    Ok(public)
}

// Only use metadata validated by public_identity from this relay's authenticated
// response. Aliases never enter the credential cache or replace subject checks.
#[cfg(test)]
fn identity_matches_username(identity: &Value, requested: &str) -> bool {
    identity["username"].as_str() == Some(requested)
        || identity["username_aliases"]
            .as_array()
            .is_some_and(|aliases| {
                aliases
                    .iter()
                    .any(|alias| alias.as_str() == Some(requested))
            })
}

pub(super) async fn open_browser(url: &Url) -> Result<()> {
    let mut command = if cfg!(windows) {
        let mut c = Command::new("rundll32.exe");
        c.args(["url.dll,FileProtocolHandler", url.as_str()]);
        c
    } else if cfg!(target_os = "android") {
        let mut c = Command::new("/system/bin/am");
        c.args([
            "start",
            "-a",
            "android.intent.action.VIEW",
            "-d",
            url.as_str(),
        ]);
        c
    } else if cfg!(target_os = "macos") {
        let mut c = Command::new("/usr/bin/open");
        c.arg(url.as_str());
        c
    } else {
        let mut c = Command::new("xdg-open");
        c.arg(url.as_str());
        c
    };
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    tokio::spawn(async move {
        let _ = child.wait().await;
    });
    Ok(())
}

#[cfg(target_os = "android")]
pub(super) async fn check_magisk_target(server: &str, username: &str) -> Result<()> {
    let api = Api::new(server)?;
    let reply = api
        .request(
            Method::GET,
            &format!("/api/client/account/{username}"),
            None,
            None,
        )
        .await?;
    if reply.status == StatusCode::NOT_FOUND {
        return Err("Account not found. Choose a registered account on this relay.".into());
    }
    let account = Api::checked(reply)?;
    if account["username"].as_str() != Some(username)
        && !account["username_aliases"]
            .as_array()
            .is_some_and(|aliases| aliases.iter().any(|alias| alias.as_str() == Some(username)))
    {
        return Err("The relay returned a different account.".into());
    }
    Ok(())
}

#[cfg(any(test, target_os = "linux"))]
fn same_subject(credential: &Credential, identity: &Value) -> Result<()> {
    if credential
        .subject
        .as_deref()
        .is_some_and(|subject| identity["subject"].as_str() != Some(subject))
    {
        return Err("the server returned a different GitHub account for this credential".into());
    }
    Ok(())
}

async fn account_info(api: &Api, username: &str) -> Result<(Value, u64)> {
    let username = crate::username::normalize(username)?;
    let reply = api
        .request(
            Method::GET,
            &format!("/api/client/account/{username}"),
            None,
            None,
        )
        .await?;
    let seconds = reply
        .date
        .and_then(|date| date.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or_else(now, |duration| duration.as_secs());
    Ok((Api::checked(reply)?, seconds))
}

pub(super) async fn monitor_announcements(server: &str, username: &str) {
    let Ok(api) = Api::new(server) else { return };
    let mut interval = tokio::time::interval(Duration::from_secs(30));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        if let Ok(Ok((info, seconds))) =
            tokio::time::timeout(Duration::from_secs(3), account_info(&api, username)).await
        {
            let lines = ["server_announcement", "announcement"].map(|key| {
                info[key]
                    .as_str()
                    .filter(|text| !text.is_empty() && text.len() <= 1024)
            });
            super::terminal::set_announcements(lines, seconds);
        }
    }
}

async fn public_whoami(args: &Arguments, target: Option<&str>) -> Result<()> {
    let (username, server) = match target {
        Some(target) => parse_target(target)?,
        None => match args.server.as_deref() {
            None if let Some(LastConnection::Relay { username, server }) = last_connection()? => {
                (username, server)
            }
            explicit => {
                let server = resolve_server(explicit)?;
                (connection_username(&server)?, server)
            }
        },
    };
    let (info, _) = account_info(&Api::new(&server)?, &username).await?;
    if args.json {
        println!("{}", json!({"server": server, "account": info}));
    } else {
        println!(
            "{}",
            crate::server_endpoint::allow_principal(
                info["username"]
                    .as_str()
                    .ok_or("server did not return an account name")?,
                &server
            )?
        );
    }
    Ok(())
}

fn standalone_interactive(args: &[OsString]) -> bool {
    let mut interactive = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].to_str() {
            Some("--server" | "--context" | "--client" | "--terminal") => {
                if args.get(index + 1).is_none() {
                    return false;
                }
                index += 2;
            }
            Some(value) if value.starts_with("--context=") => index += 1,
            Some("--interactive") => {
                interactive = true;
                index += 1;
            }
            Some("-i") => {
                if args
                    .get(index + 1)
                    .is_some_and(|arg| !arg.to_string_lossy().starts_with('-'))
                {
                    return false;
                }
                interactive = true;
                index += 1;
            }
            Some("--json" | "--help" | "-h") => index += 1,
            _ => return false,
        }
    }
    interactive
}

/// A valued -i remains an SSH identity for an incoming controlled connection.
pub(super) async fn dispatch(args: &[OsString]) -> Result<bool> {
    let Some((command, rest)) = args.split_first() else {
        return Ok(false);
    };
    match command.to_str() {
        Some("list" | "ls") => crate::session::list_command(rest).await?,
        Some("attach") => crate::session::attach::manage(rest.to_vec()).await?,
        Some("send" | "wait" | "logout") => return Err("this device accepts control through its authorized controller; use the controller for account and remote operations".into()),
        _ if standalone_interactive(args) => return Err("interactive control of remote devices is unavailable in the device client; use ssmcp attach NAME for a local session".into()),
        Some("whoami") => {
            let (target, flags) = match rest.split_first() {
                Some((target, flags)) if target.to_str().is_some_and(|t| t.contains('@')) =>
                    (target.to_str(), flags),
                _ => (None, rest),
            };
            let mut options = Arguments::parse(flags)?;
            if options.no_browser || options.name.is_some() {
                return Err("usage: ssmcp whoami [USERNAME@SERVER | --context NAME | --server URL] [--json]".into());
            }
            if options.help {
                println!("Usage: ssmcp whoami [USERNAME@SERVER | --context NAME | --server URL] [--json]\nShow public relay account information for the selected device destination.");
                return Ok(true);
            }
            if target.is_some() {
                if options.server.is_some() || options.context.is_some() {
                    return Err("an explicit account cannot be combined with a server or context selector".into());
                }
            } else {
                options.server = super::connection_context::remote_server(
                    options.context.as_deref(), options.server.as_deref()
                )?;
            }
            public_whoami(&options, target).await?;
        }
        _ => return Ok(false),
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn temporary() -> PathBuf {
        let mut random = [0; 16];
        platform::random_bytes(&mut random).unwrap();
        let root = std::env::temp_dir().join(format!(
            "ssmcp-auth-{}",
            crate::hex::encode(Sha256::digest(random))
        ));
        create_tree(&root).unwrap();
        root
    }
    fn credential(server: &str) -> Credential {
        Credential {
            server: server.into(),
            token: "a".repeat(64),
            principal: "alice@github".into(),
            subject: Some("123".into()),
            username: None,
            expires_at: now() + 3600,
        }
    }
    #[test]
    fn interactive_flag_order_preserves_valued_ssh_identity() {
        let values = |args: &[&str]| args.iter().map(OsString::from).collect::<Vec<_>>();
        for args in [
            vec!["-i"],
            vec!["-i", "--server", "https://example.com"],
            vec!["--server", "https://example.com", "-i"],
            vec!["--server", "https://example.com", "--interactive"],
            vec!["--client", "client-id", "-i", "--terminal", "main"],
        ] {
            assert!(standalone_interactive(&values(&args)), "{args:?}");
        }
        for args in [
            vec!["-i", "id_ed25519", "host"],
            vec![
                "--server",
                "https://example.com",
                "-i",
                "id_ed25519",
                "host",
            ],
            vec!["--identity", "id_ed25519", "host"],
            vec!["--server", "https://example.com"],
            vec!["--server", "-i"],
        ] {
            assert!(!standalone_interactive(&values(&args)), "{args:?}");
        }
    }

    #[test]
    fn origin_validation_prevents_cross_origin_or_plaintext_delivery() {
        assert_eq!(
            normalize_server("https://EXAMPLE.com:443/").unwrap(),
            "https://example.com"
        );
        assert_eq!(
            normalize_server("http://127.0.0.1:34123").unwrap(),
            "http://127.0.0.1:34123"
        );
        assert!(normalize_server("http://[::1]:34123").is_ok());
        for url in [
            "http://example.com",
            "http://localhost",
            "https://user@example.com",
            "https://example.com/path",
            "https://example.com?x=1",
            "https://example.com/#fragment",
            "file:///tmp/auth",
        ] {
            assert!(normalize_server(url).is_err(), "{url}");
        }
    }
    #[test]
    fn accounts_are_origin_scoped_and_expiry_is_enforced() {
        let root = temporary();
        let one = credential("https://one.example");
        let mut two = credential("https://two.example");
        two.principal = "bob@github".into();
        save_at(&root, &one).unwrap();
        save_at(&root, &two).unwrap();
        assert_eq!(
            load_at(&root, None, false).unwrap().unwrap().principal,
            "bob@github"
        );
        assert_eq!(
            load_at(&root, Some("https://one.example"), false)
                .unwrap()
                .unwrap()
                .principal,
            "alice@github"
        );
        assert!(
            load_at(&root, Some("https://three.example"), false)
                .unwrap()
                .is_none()
        );
        two.expires_at = now() - 1;
        save_at(&root, &two).unwrap();
        assert!(load_at(&root, None, false).is_err());
        assert!(load_at(&root, None, true).unwrap().is_some());
        fs::remove_dir_all(root).unwrap();
    }
    #[cfg(unix)]
    #[test]
    fn credentials_reject_public_permissions_and_symlinks() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let root = temporary();
        let value = credential("https://one.example");
        save_at(&root, &value).unwrap();
        let path = credential_path(&root, &value.server);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(load_at(&root, None, false).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let link = root.join("linked");
        symlink(&path, &link).unwrap();
        assert!(read_private(&link).is_err());
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(load_at(&root, None, false).is_err());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn saved_identity_cannot_be_substituted_between_origins() {
        let root = temporary();
        let value = credential("https://one.example");
        save_at(&root, &value).unwrap();
        write_private(
            &credential_path(&root, "https://two.example"),
            &serde_json::to_vec(&value).unwrap(),
        )
        .unwrap();
        assert!(load_at(&root, Some("https://two.example"), false).is_err());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn duplicate_account_servers_are_rejected() {
        let args = [
            "--server",
            "https://one.example",
            "--server",
            "https://two.example",
        ]
        .map(OsString::from);
        assert!(Arguments::parse(&args).is_err());
    }

    #[test]
    fn identity_output_and_debug_exclude_tokens() {
        let value = json!({"identity":{"provider":"github","subject":"123","login":"Alice","principal":"alice@github","token":"do-not-print"},"access_token":"never"});
        assert_eq!(
            public_identity(&value).unwrap(),
            json!({"provider":"github","subject":"123","login":"Alice","principal":"alice@github"})
        );
        assert!(!format!("{:?}", credential("https://example.com")).contains(&"a".repeat(64)));
        assert!(public_identity(&json!({"provider":"github","subject":"123","login":"alice","principal":"bob@github"})).is_err());
    }

    #[test]
    fn username_claim_metadata_is_optional_bounded_and_canonical() {
        let legacy =
            json!({"provider":"github","subject":"123","login":"alice","username":"current-name"});
        let public = public_identity(&legacy).unwrap();
        assert!(public.get("username_aliases").is_none());
        assert!(public.get("username_generated").is_none());
        assert!(identity_matches_username(&public, "current-name"));
        assert!(!identity_matches_username(&public, "former-name"));

        let aliases: Vec<_> = (0..MAX_USERNAME_ALIASES)
            .map(|index| format!("former-{index}"))
            .collect();
        let mut identity = legacy;
        identity["username_aliases"] = json!(aliases);
        identity["username_generated"] = json!(false);
        identity["token"] = json!("never-expose");
        let public = public_identity(&json!({"identity":identity})).unwrap();
        assert_eq!(public["username_aliases"].as_array().unwrap().len(), 128);
        assert!(identity_matches_username(&public, "former-127"));
        assert!(!identity_matches_username(&public, "unrelated"));
        assert_eq!(public["username_generated"], false);
        assert!(public.get("token").is_none());
    }

    #[test]
    fn invalid_username_claim_metadata_is_rejected_before_matching() {
        let base =
            json!({"provider":"github","subject":"123","login":"alice","username":"current-name"});
        for aliases in [
            Value::Null,
            json!(false),
            json!("former-name"),
            json!({"name":"former-name"}),
            json!([null]),
            json!([7]),
            json!(["Former-Name"]),
            json!(["bad name"]),
            json!(["admin"]),
            json!(vec!["former-name"; MAX_USERNAME_ALIASES + 1]),
        ] {
            let mut identity = base.clone();
            identity["username_aliases"] = aliases;
            assert!(public_identity(&identity).is_err());
        }
        for generated in [Value::Null, json!("false"), json!(0), json!([])] {
            let mut identity = base.clone();
            identity["username_generated"] = generated;
            assert!(public_identity(&identity).is_err());
        }
        let mut unbound = base;
        unbound.as_object_mut().unwrap().remove("username");
        unbound["username_aliases"] = json!(["former-name"]);
        assert!(public_identity(&unbound).is_err());
        unbound["username_aliases"] = json!([]);
        unbound["username_generated"] = json!(true);
        assert!(public_identity(&unbound).is_err());
        unbound["username_generated"] = json!(false);
        assert!(public_identity(&unbound).is_ok());
    }

    #[tokio::test]
    async fn api_does_not_follow_redirects_with_account_tokens() {
        use tokio::{
            io::{AsyncReadExt, AsyncWriteExt},
            net::TcpListener,
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = [0; 4096];
            let mut request = Vec::new();
            loop {
                let read = socket.read(&mut bytes).await.unwrap();
                assert_ne!(read, 0, "client closed before sending request headers");
                request.extend_from_slice(&bytes[..read]);
                assert!(request.len() <= 8192, "fixture request headers too large");
                if request.windows(4).any(|part| part == b"\r\n\r\n") {
                    break;
                }
            }
            socket.write_all(b"HTTP/1.1 302 Found\r\nLocation: https://other.example/private\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
        });
        let api = Api::new(&format!("http://{addr}")).unwrap();
        assert!(
            api.get("/api/client/whoami", &"a".repeat(64))
                .await
                .is_err()
        );
        server.await.unwrap();
    }
}

#[cfg(test)]
#[path = "account/named_tests.rs"]
mod named_tests;
