//! Account sign-in, inspection, and authenticated remote control.
use crate::{Result, platform, session::attach::input};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use reqwest::{Client, Method, StatusCode, Url};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    ffi::OsString,
    fs::{self, OpenOptions},
    io::{self, IsTerminal, Read, Write},
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::process::Command;

mod attach;
mod chat;
mod directory;
pub(super) mod login;
mod send;
pub(super) mod status;
mod wait;

pub(super) const DEFAULT_SERVER: &str = "https://ssmcp.lmm.best";
const STORE_LIMIT: usize = 32 * 1024;
const RESPONSE_LIMIT: usize = 8 * 1024 * 1024;

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

fn valid_secret(value: &str) -> bool {
    (32..=1024).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

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
        return Err("invalid saved account credential; run ssmcp login again".into());
    }
    Ok(())
}

fn state_root() -> Result<PathBuf> {
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

fn create_tree(path: &Path) -> Result<()> {
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

fn verify_directory(path: &Path) -> Result<()> {
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

fn read_private_limit(path: &Path, limit: usize) -> Result<Option<Vec<u8>>> {
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

fn write_private_limit(path: &Path, bytes: &[u8], limit: usize) -> Result<()> {
    if bytes.len() > limit {
        return Err("account credential exceeds its size limit".into());
    }
    let mut random = [0; 16];
    platform::random_bytes(&mut random)?;
    let suffix: String = random.iter().map(|b| format!("{b:02x}")).collect();
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
        return Err("account login has expired; run ssmcp login again".into());
    }
    Ok(Some(value))
}

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
pub(super) async fn credentials(server: Option<&str>) -> Result<Option<Credential>> {
    let server = resolve_server(server)?;
    load_at(&state_root()?, Some(&server), false)
}

#[derive(Clone)]
struct Api {
    client: Client,
    server: String,
}
struct Reply {
    status: StatusCode,
    value: Value,
    retry_after: u64,
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
        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse().ok())
            .unwrap_or(2u64)
            .clamp(1, 60);
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
            value,
            retry_after,
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
    async fn get(&self, route: &str, token: &str) -> Result<Value> {
        Self::checked(self.request(Method::GET, route, Some(token), None).await?)
    }
    async fn post(&self, route: &str, token: Option<&str>, body: Value) -> Result<Value> {
        Self::checked(self.request(Method::POST, route, token, Some(body)).await?)
    }
    async fn call(
        &self,
        credential: &Credential,
        id: &str,
        name: &str,
        arguments: Value,
    ) -> Result<Value> {
        let result = self
            .post(
                "/api/client/call",
                Some(&credential.token),
                json!({"client_id":id,"name":name,"arguments":arguments}),
            )
            .await?;
        if result["isError"] == true {
            return Err(format!("remote tool {name} failed; check the selected session").into());
        }
        if let Some(value) = result.get("structuredContent") {
            return Ok(value.clone());
        }
        if let Some(parts) = result["content"].as_array() {
            for part in parts {
                if part["type"] == "text"
                    && let Some(text) = part["text"].as_str()
                    && let Ok(value) = serde_json::from_str(text)
                {
                    return Ok(value);
                }
            }
        }
        Ok(result)
    }
}

#[derive(Default)]
struct Arguments {
    server: Option<String>,
    context: Option<String>,
    json: bool,
    interactive: bool,
    remote: bool,
    help: bool,
    no_browser: bool,
    name: Option<String>,
    client: Option<String>,
    terminal: Option<String>,
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
                Some("-i" | "--interactive") => result.interactive = true,
                Some("--remote") => result.remote = true,
                Some("--help" | "-h") => result.help = true,
                Some("--no-browser") => result.no_browser = true,
                Some("--name") => result.name = Some(value()?),
                Some("--client") => result.client = Some(value()?),
                Some("--terminal") => result.terminal = Some(value()?),
                _ => return Err("unknown account option; see ssmcp login --help".into()),
            }
        }
        if result.json && result.interactive {
            return Err("--json cannot be combined with interactive mode".into());
        }
        Ok(result)
    }
}

fn safe(value: &str) -> String {
    value
        .chars()
        .filter(|c| !c.is_control())
        .take(256)
        .collect()
}

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
    Ok(public)
}

fn verification_url(server: &str, raw: &str, secret: &str) -> Result<Url> {
    let base = Url::parse(server)?;
    let url = Url::parse(raw).map_err(|_| "server returned an invalid login URL")?;
    if url.origin() != base.origin()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.as_str().contains(secret)
    {
        return Err("server returned an unsafe login URL".into());
    }
    Ok(url)
}

pub(super) async fn open_browser(url: &Url) -> Result<()> {
    let mut command = if cfg!(windows) {
        let mut c = Command::new("rundll32.exe");
        c.args(["url.dll,FileProtocolHandler", url.as_str()]);
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

struct Authorization {
    api: Api,
    server: String,
    code: String,
    url: Url,
    user_code: Option<String>,
    username: Option<String>,
    interval: u64,
    deadline: tokio::time::Instant,
}

async fn begin_authorization(
    root: &Path,
    args: &Arguments,
    username: Option<&str>,
) -> Result<Authorization> {
    let username = username.map(crate::username::normalize).transpose()?;
    if root.exists() {
        verify_directory(root)?;
    }
    let server = resolve_server(args.server.as_deref())?;
    let api = Api::new(&server)?;
    let name = args
        .name
        .clone()
        .unwrap_or_else(|| platform::hostname().unwrap_or_else(|_| "ssmcp client".into()));
    if name.trim().is_empty() || name.len() > 80 || name.chars().any(char::is_control) {
        return Err("--name must contain 1..=80 bytes without control characters".into());
    }
    let start = api
        .post(
            "/api/client/login/start",
            None,
            json!({"name":name,"username":username}),
        )
        .await?;
    let code = start["device_code"]
        .as_str()
        .filter(|v| valid_secret(v))
        .ok_or("invalid device login code")?
        .to_owned();
    let raw_url = start["verification_uri_complete"]
        .as_str()
        .or_else(|| start["verification_uri"].as_str())
        .ok_or("server omitted the login URL")?;
    let url = verification_url(&server, raw_url, &code)?;
    let expires = start["expires_in"]
        .as_u64()
        .filter(|n| (1..=1800).contains(n))
        .ok_or("invalid device login lifetime")?;
    let interval = start["interval"].as_u64().unwrap_or(2).clamp(1, 60);
    // Removing controls must not reassemble the private polling code for display.
    let user_code = start["user_code"]
        .as_str()
        .map(safe)
        .filter(|value| !value.contains(code.as_str()));
    Ok(Authorization {
        api,
        server,
        code,
        url,
        user_code,
        username,
        interval,
        deadline: tokio::time::Instant::now() + Duration::from_secs(expires),
    })
}

async fn finish_authorization(root: &Path, authorization: Authorization) -> Result<Credential> {
    let Authorization {
        api,
        server,
        code,
        username,
        mut interval,
        deadline,
        ..
    } = authorization;
    let mut interrupt = platform::interrupt_signal()?;
    let mut terminate = platform::terminate_signal()?;
    let response = loop {
        let reply = tokio::select! {
            result = api.request(Method::POST, "/api/client/login/poll", None, Some(json!({"device_code":code}))) => result?,
            _ = interrupt.recv() => return Err("login cancelled".into()),
            _ = terminate.recv() => return Err("login cancelled".into()),
            _ = tokio::time::sleep_until(deadline) => return Err("authorization link expired; run the connection command again".into()),
        };
        let state = reply.value["status"]
            .as_str()
            .or_else(|| reply.value["error"].as_str())
            .unwrap_or("");
        if reply.status.is_success() && reply.value["access_token"].is_string() {
            break reply.value;
        }
        if !matches!(state, "pending" | "authorization_pending" | "slow_down") {
            if matches!(state, "denied" | "access_denied") {
                return Err("GitHub login was declined".into());
            }
            if matches!(state, "expired" | "expired_token") {
                return Err("authorization link expired; run the connection command again".into());
            }
            if !reply.status.is_success() {
                Api::checked(reply)?;
            }
            return Err("device authorization failed; run the connection command again".into());
        }
        if state == "slow_down" || reply.status == StatusCode::TOO_MANY_REQUESTS {
            interval = interval.saturating_add(1).max(reply.retry_after).min(60);
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(interval)) => {},
            _ = interrupt.recv() => return Err("login cancelled".into()),
            _ = terminate.recv() => return Err("login cancelled".into()),
            _ = tokio::time::sleep_until(deadline) => return Err("authorization link expired; run the connection command again".into()),
        }
    };
    let identity = public_identity(&response)?;
    if username
        .as_deref()
        .is_some_and(|requested| identity["username"].as_str() != Some(requested))
    {
        return Err(
            "GitHub authorization did not bind the requested username; no credential was saved"
                .into(),
        );
    }
    let token = response["access_token"]
        .as_str()
        .filter(|v| valid_secret(v))
        .ok_or("invalid account token")?;
    let expires = response["expires_in"]
        .as_u64()
        .filter(|n| (1..=366 * 86400).contains(n))
        .ok_or("invalid account login lifetime")?;
    let credential = Credential {
        server,
        token: token.into(),
        principal: identity["principal"].as_str().unwrap().into(),
        subject: Some(identity["subject"].as_str().unwrap().into()),
        username: identity["username"].as_str().map(str::to_owned),
        expires_at: now().saturating_add(expires),
    };
    save_at(root, &credential)?;
    Ok(credential)
}

async fn authorize_at(root: &Path, args: &Arguments, username: Option<&str>) -> Result<Credential> {
    let authorization = begin_authorization(root, args, username).await?;
    eprintln!(
        "Open this link to sign in with GitHub:\n{}",
        authorization.url
    );
    if let Some(user_code) = &authorization.user_code {
        eprintln!("Confirmation code: {user_code}");
    }
    if !args.no_browser && open_browser(&authorization.url).await.is_err() {
        eprintln!("Open the printed link in your browser.");
    }
    finish_authorization(root, authorization).await
}

async fn login(args: &Arguments) -> Result<()> {
    let credential = authorize_at(&state_root()?, args, None).await?;
    let mut identity = json!({
        "provider":"github",
        "subject":credential.subject,
        "login":credential.principal.trim_end_matches("@github"),
        "principal":credential.principal,
    });
    if let Some(username) = &credential.username {
        identity["username"] = json!(username);
    }
    if args.json {
        println!(
            "{}",
            json!({"server":credential.server,"identity":identity,"expires_at":credential.expires_at})
        );
    } else {
        println!(
            "Signed in as {} · {}",
            credential.principal, credential.server
        );
    }
    Ok(())
}

/// Explicit old-relay compatibility is limited to an existing, verified self account.
/// This path is read-only: no OAuth, username claim or credential-file update.
pub(super) async fn compat_credential(server: &str, allow: &[String]) -> Result<Credential> {
    compat_at(&state_root()?, server, allow).await
}

async fn compat_at(root: &Path, server: &str, allow: &[String]) -> Result<Credential> {
    let [principal] = allow else {
        return Err("--account-compat requires exactly one allowed account".into());
    };
    let server = normalize_server(server)?;
    let requested = crate::server_endpoint::allow_handle_for_server(principal, &server)?;
    let mut credential = load_at(root, Some(&server), false)?.ok_or(
        "--account-compat requires an existing saved login for this relay; sign in separately",
    )?;
    let reply = Api::new(&server)?
        .get("/api/client/whoami", &credential.token)
        .await?;
    let identity = public_identity(&reply)?;
    same_subject(&credential, &identity)?;
    if identity["principal"].as_str() != Some(credential.principal.as_str()) {
        return Err("--account-compat server identity differs from the saved account".into());
    }
    if identity["username"].as_str() != Some(requested.as_str()) {
        return Err("--account-compat requires the saved account's registered relay username to match the only --allow entry".into());
    }
    credential.subject = Some(identity["subject"].as_str().unwrap().into());
    credential.username = Some(requested);
    Ok(credential)
}

/// Obtain and validate the selected server's account without a separate login command.
pub(super) async fn connect_credential(
    server: Option<&str>,
    username: Option<&str>,
    no_browser: bool,
    allow_login: bool,
) -> Result<Credential> {
    let server = resolve_server(server)?;
    connect_at(
        &state_root()?,
        Some(&server),
        username,
        no_browser,
        allow_login,
    )
    .await
}

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

async fn connect_at(
    root: &Path,
    server: Option<&str>,
    username: Option<&str>,
    no_browser: bool,
    allow_login: bool,
) -> Result<Credential> {
    let requested = username.map(crate::username::normalize).transpose()?;
    let server = selected_server(root, server)?;
    let saved = load_at(root, Some(&server), true)?;
    let api = Api::new(&server)?;
    if let Some(credential) = saved.as_ref().filter(|value| value.expires_at > now()) {
        let reply = api
            .request(
                Method::GET,
                "/api/client/whoami",
                Some(&credential.token),
                None,
            )
            .await?;
        // Only rejected/expired tokens restart OAuth. Outages and bans retain
        // the saved credential and return their own error.
        if reply.status != StatusCode::UNAUTHORIZED {
            let mut identity = public_identity(&Api::checked(reply)?)?;
            same_subject(credential, &identity)?;
            if identity["username"].is_null()
                && let Some(username) = requested.as_deref()
            {
                identity = public_identity(
                    &api.post(
                        "/api/client/username",
                        Some(&credential.token),
                        json!({"username":username}),
                    )
                    .await?,
                )?;
                same_subject(credential, &identity)?;
            }
            if let Some(requested) = requested.as_deref()
                && identity["username"].as_str() != Some(requested)
            {
                return Err("this account is registered with a different username; use its registered username or sign out with ssmcp logout to switch accounts".into());
            }
            let mut current = credential.clone();
            current.subject = Some(identity["subject"].as_str().unwrap().into());
            current.principal = identity["principal"].as_str().unwrap().into();
            current.username = identity["username"].as_str().map(str::to_owned);
            save_at(root, &current)?;
            return Ok(current);
        }
    }
    let username = requested
        .as_deref()
        .or_else(|| saved.as_ref().and_then(|value| value.username.as_deref()));
    if !allow_login {
        return Err("this background session needs authorization; run ssmcp username@server in a terminal, then restart the session".into());
    }
    if saved.is_none() && username.is_none() {
        return Err(format!(
            "choose a username to connect: ssmcp username@{}",
            server.strip_prefix("https://").unwrap_or(&server)
        )
        .into());
    }
    authorize_at(
        root,
        &Arguments {
            server: Some(server),
            no_browser,
            ..Arguments::default()
        },
        username,
    )
    .await
}

async fn require_credential(server: Option<&str>) -> Result<Credential> {
    credentials(server)
        .await?
        .ok_or_else(|| "not signed in; run ssmcp login [--server URL]".into())
}

async fn whoami(args: &Arguments) -> Result<()> {
    let credential = require_credential(args.server.as_deref()).await?;
    let reply = Api::new(&credential.server)?
        .get("/api/client/whoami", &credential.token)
        .await?;
    let identity = public_identity(&reply)?;
    if identity["principal"] != credential.principal {
        return Err("server identity differs from the saved login; run ssmcp login again".into());
    }
    if args.json {
        println!(
            "{}",
            json!({"server":credential.server,"identity":identity,"expires_at":credential.expires_at})
        );
    } else {
        if let Some(username) = identity["username"].as_str() {
            println!(
                "{username} · {} · {}",
                credential.principal, credential.server
            );
        } else {
            println!("{} · {}", credential.principal, credential.server);
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum LogoutStatus {
    NotSignedIn,
    Revoked,
    AlreadyInvalid,
    LocalOnly,
}

#[derive(Debug)]
pub(super) struct LogoutOutcome {
    pub(super) server: String,
    pub(super) status: LogoutStatus,
}

impl LogoutOutcome {
    pub(super) fn message(&self) -> String {
        match self.status {
            LogoutStatus::NotSignedIn => format!("Not signed in to {}.", self.server),
            LogoutStatus::Revoked => format!(
                "Signed out of {}. This login and its previous delegated credentials were revoked.",
                self.server
            ),
            LogoutStatus::AlreadyInvalid => format!(
                "Signed out of {}. The saved login was already expired or revoked.",
                self.server
            ),
            LogoutStatus::LocalOnly => format!(
                "Signed out locally from {}. Remote revocation could not be confirmed; this login and its derived devices may remain valid until they expire.",
                self.server
            ),
        }
    }

    fn json(&self) -> Value {
        json!({"server":self.server,"logged_out":true,"status":self.status})
    }
}

/// Clear only the selected origin's saved login, then try to revoke that login.
/// An Ok result always means local logout completed; LocalOnly leaves remote
/// revocation unconfirmed. Callers must also close their in-memory connection.
pub(super) async fn logout(server: Option<&str>) -> Result<LogoutOutcome> {
    let server = resolve_server(server)?;
    let Some(credential) = load_at(&state_root()?, Some(&server), true)? else {
        return Ok(LogoutOutcome {
            server,
            status: LogoutStatus::NotSignedIn,
        });
    };
    logout_connection(&server, &credential.token).await
}

/// A terminal must use the credential snapshot that opened its connection.
/// A changed or missing saved login is refused without sending a revoke request.
pub(super) async fn logout_connection(server: &str, expected_token: &str) -> Result<LogoutOutcome> {
    if !valid_secret(expected_token) {
        return Err("this connection has no valid original login credential".into());
    }
    let server = normalize_server(server)?;
    logout_at(&state_root()?, &server, Some(expected_token)).await
}

async fn logout_at(
    root: &Path,
    server: &str,
    expected_token: Option<&str>,
) -> Result<LogoutOutcome> {
    let Some(credential) = load_at(root, Some(server), true)? else {
        if expected_token.is_some() {
            return Err(
                "no saved login for this connection; remote revocation was not attempted".into(),
            );
        }
        return Ok(LogoutOutcome {
            server: server.into(),
            status: LogoutStatus::NotSignedIn,
        });
    };
    if expected_token.is_some_and(|token| token != credential.token.as_str()) {
        return Err(
            "saved login changed; it was preserved and remote revocation was not attempted".into(),
        );
    }
    // Delete before the network await: cancellation or an offline server must
    // not retain this login, nor remove a new login saved while revoking it.
    match fs::remove_file(credential_path(root, &credential.server)) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let status = match Api::new(&credential.server) {
        Ok(api) => match api
            .request(
                Method::POST,
                "/api/client/logout",
                Some(&credential.token),
                Some(json!({})),
            )
            .await
        {
            Ok(reply) if reply.status.is_success() => LogoutStatus::Revoked,
            Ok(reply) if reply.status == StatusCode::UNAUTHORIZED => LogoutStatus::AlreadyInvalid,
            _ => LogoutStatus::LocalOnly,
        },
        Err(_) => LogoutStatus::LocalOnly,
    };
    Ok(LogoutOutcome {
        server: credential.server,
        status,
    })
}

async fn logout_command(args: &Arguments) -> Result<()> {
    let outcome = logout(args.server.as_deref()).await?;
    if args.json {
        println!("{}", outcome.json());
    } else {
        println!("{}", outcome.message());
    }
    Ok(())
}

fn clients(value: &Value) -> Result<&[Value]> {
    value["clients"]
        .as_array()
        .map(Vec::as_slice)
        .ok_or_else(|| "server returned an invalid session list".into())
}
fn list_table(entries: &[Value]) -> String {
    if entries.is_empty() {
        return "No controllable sessions are online.\n".into();
    }
    let mut text = String::from("  #  NAME                     HOST                 SESSION\n");
    for (index, entry) in entries.iter().enumerate() {
        text.push_str(&format!(
            "{:>3}  {:<24} {:<20} {}\n",
            index + 1,
            safe(entry["name"].as_str().unwrap_or("?")),
            safe(entry["hostname"].as_str().unwrap_or("?")),
            safe(entry["id"].as_str().unwrap_or("?"))
        ));
        if let Some(title) = entry["title"].as_str() {
            text.push_str(&format!("     {}\n", safe(title)));
        }
    }
    text
}
fn select_client<'a>(entries: &'a [Value], selected: &str) -> Result<&'a str> {
    if let Ok(index) = selected.parse::<usize>()
        && index > 0
        && let Some(id) = entries.get(index - 1).and_then(|v| v["id"].as_str())
    {
        return Ok(id);
    }
    select_client_exact(entries, selected)
}

fn select_client_exact<'a>(entries: &'a [Value], selected: &str) -> Result<&'a str> {
    let matches: Vec<_> = entries
        .iter()
        .filter(|entry| {
            entry["id"].as_str() == Some(selected) || entry["name"].as_str() == Some(selected)
        })
        .collect();
    if matches.len() != 1 {
        return Err("select a listed session number or its exact unique ID/name".into());
    }
    matches[0]["id"]
        .as_str()
        .ok_or_else(|| "session has no ID".into())
}
fn interactive_selection_help() -> &'static str {
    "Choose an online session to open its existing remote terminal.\nEnter a listed number, exact ID or name; Ctrl+C cancels selection.\nOnce connected, Ctrl+] returns to your local terminal; the remote session and its jobs keep running."
}

fn interactive_connection_hint(id: &str, terminal: &str) -> String {
    format!(
        "Connected to {} / {} · Ctrl+] returns to your local terminal; remote session keeps running · Ctrl+C reaches the remote shell",
        safe(id),
        safe(terminal)
    )
}

async fn prompt_selection() -> Result<String> {
    eprint!("Choose a session number, exact ID or name: ");
    io::stderr().flush()?;
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        use std::io::BufRead;
        let mut value = String::new();
        let result = io::stdin()
            .lock()
            .take(4097)
            .read_line(&mut value)
            .map_err(crate::Error::from)
            .and_then(|n| {
                if n == 0 || value.len() > 4096 {
                    Err("session selection cancelled".into())
                } else {
                    Ok(value.trim().to_owned())
                }
            });
        let _ = tx.send(result);
    });
    let mut interrupt = platform::interrupt_signal()?;
    tokio::select! {
        result=rx=>result.map_err(|_|"could not read session selection")?,
        _=interrupt.recv()=>Err("session selection cancelled".into()),
    }
}
async fn list(args: &Arguments) -> Result<()> {
    let credential = require_credential(args.server.as_deref()).await?;
    let api = Api::new(&credential.server)?;
    let value = api.get("/api/client/sessions", &credential.token).await?;
    let entries = clients(&value)?;
    if !args.interactive {
        let directory = directory::Directory::parse(&value)?;
        directory.remember(&credential)?;
        if args.json {
            let mut value = value.clone();
            value["selection"] = directory.selection_json();
            println!("{}", serde_json::to_string_pretty(&value)?);
        } else {
            print!("{}", directory.table());
        }
        return Ok(());
    }
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(
            "interactive mode requires a terminal; use ssmcp list --json for scripts".into(),
        );
    }
    eprintln!("{}", interactive_selection_help());
    if entries.is_empty() {
        print!("{}", list_table(entries));
        return Ok(());
    }
    let selected = if let Some(id) = &args.client {
        id.clone()
    } else {
        eprintln!(
            "This PTY chooser uses its own immediate numbering; ssmcp list numbers remain unchanged."
        );
        eprint!("{}", list_table(entries));
        prompt_selection().await?
    };
    let id = select_client(entries, &selected)?;
    interactive(
        &api,
        &credential,
        id,
        args.terminal.as_deref().unwrap_or("main"),
        false,
    )
    .await
}

struct Reader(tokio::task::JoinHandle<()>);
impl Drop for Reader {
    fn drop(&mut self) {
        self.0.abort();
    }
}
async fn interactive(
    api: &Api,
    credential: &Credential,
    id: &str,
    terminal: &str,
    read_only: bool,
) -> Result<()> {
    if terminal.is_empty() || terminal.len() > 128 || terminal.chars().any(char::is_control) {
        return Err("--terminal must be a listed terminal ID".into());
    }
    api.call(
        credential,
        id,
        "terminal.status",
        json!({"terminal_id":terminal}),
    )
    .await?;
    let mut keyboard = input::Keyboard::open(read_only)?;
    let mut utf8 = input::Utf8Input::default();
    if read_only {
        eprintln!(
            "Connected to {} / {} · Read-only: input and terminal resize are disabled; Ctrl+] or Ctrl+C detaches without stopping the remote session.",
            safe(id),
            safe(terminal)
        );
    } else {
        eprintln!("{}", interactive_connection_hint(id, terminal));
    }
    let (tx, mut rx) = tokio::sync::mpsc::channel(2);
    let (reader_api, reader_credential, reader_id, reader_terminal) = (
        api.clone(),
        credential.clone(),
        id.to_owned(),
        terminal.to_owned(),
    );
    let reader = Reader(tokio::spawn(async move {
        let mut cursor = 0;
        loop {
            let page=reader_api.call(&reader_credential,&reader_id,"terminal.read",
                json!({"terminal_id":reader_terminal,"cursor":cursor,"max_bytes":32768,"wait_ms":5000})).await
                .and_then(|page|{cursor=page["next_cursor"].as_u64().ok_or("remote terminal omitted its cursor")?;Ok(page)})
                .map_err(|e|e.to_string());
            let end = page.as_ref().is_ok_and(|p| p["output_eof"] == true) || page.is_err();
            if tx.send(page).await.is_err() || end {
                break;
            }
        }
    }));
    let mut terminate = platform::terminate_signal()?;
    let result:Result<()>=async{
        loop{
            tokio::select!{
                _=terminate.recv()=>break,
                event=keyboard.next()=>match event?{
                    input::Event::Detach=>break,
                    input::Event::InputProgress(_)=>{}
                    input::Event::PasteExpired=>{utf8=input::Utf8Input::default();}
                    input::Event::End=>{
                        let text=utf8.push(&[],true);
                        if !read_only && !text.is_empty(){api.call(credential,id,"terminal.write",json!({"terminal_id":terminal,"data":text})).await?;}
                        break;
                    }
                    input::Event::Interrupt=>{
                        if read_only { break; }
                        // This interactive command owns a remote shell, not a
                        // dashboard. Preserve the advertised remote Ctrl-C.
                        let text=utf8.push(b"\x03",false);
                        api.call(credential,id,"terminal.write",json!({"terminal_id":terminal,"data":text})).await?;
                    }
                    input::Event::Bytes(bytes)|input::Event::PasteBytes(bytes)=>{
                        if !read_only {
                            let text=utf8.push(&bytes,false);
                            if !text.is_empty(){api.call(credential,id,"terminal.write",json!({"terminal_id":terminal,"data":text})).await?;}
                        }
                    }
                    input::Event::Resize(rows,cols)=>{
                        if !read_only {
                            api.call(credential,id,"terminal.resize",json!({"terminal_id":terminal,"rows":rows.clamp(1,1000),"cols":cols.clamp(1,1000)})).await?;
                        }
                    }
                },
                page=rx.recv()=>{
                    let page=page.ok_or("remote terminal output ended")?.map_err(crate::Error::from)?;
                    let bytes=if let Some(encoded)=page["data_base64"].as_str(){STANDARD.decode(encoded)?}
                        else{page["text"].as_str().ok_or("invalid terminal output")?.as_bytes().to_vec()};
                    io::stdout().write_all(&bytes)?;io::stdout().flush()?;
                    if page["output_eof"]==true{break;}
                },
            }
        }Ok(())
    }.await;
    drop(reader);
    drop(keyboard);
    eprintln!("\r\nDetached; the remote session remains running.");
    result
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

fn remote_session_args(args: &[OsString]) -> Option<&[OsString]> {
    if !args.first().is_some_and(|arg| arg == "sessions") {
        return None;
    }
    let rest = if args.get(1).is_some_and(|arg| arg == "list") {
        &args[2..]
    } else {
        &args[1..]
    };
    // The same arity-aware parser distinguishes selectors from --title/--client
    // values. A saved-session command such as create is never an account list.
    let parsed = Arguments::parse(rest).ok()?;
    (parsed.remote || parsed.interactive).then_some(rest)
}

/// Account commands run before legacy parsing; a valued -i keeps SSH semantics.
pub(super) async fn dispatch(args: &[OsString]) -> Result<bool> {
    let Some(first) = args.first().and_then(|v| v.to_str()) else {
        return Ok(false);
    };
    if first == "send" {
        send::run(&args[1..]).await?;
        return Ok(true);
    }
    if first == "wait" {
        wait::run(args).await?;
        return Ok(true);
    }
    if first == "attach" {
        attach::run(&args[1..]).await?;
        return Ok(true);
    }
    let remote_sessions = remote_session_args(args);
    let (operation, rest, interactive) = match first {
        "login" | "logout" | "whoami" | "list" | "ls" => (first, &args[1..], false),
        "sessions" if remote_sessions.is_some() => ("list", remote_sessions.unwrap(), false),
        _ if standalone_interactive(args) => ("list", args, true),
        _ => return Ok(false),
    };
    let mut options = Arguments::parse(rest)?;
    if options.help {
        println!(
            "ssmcp login [--context NAME | --server URL] [--no-browser] [--name NAME]\nssmcp whoami [--context NAME | --server URL] [--json]\nssmcp logout [--context NAME | --server URL] [--json]\nssmcp list [--context NAME | --server URL] [--json | -i] [--client ID] [--terminal ID]\nssmcp -i [--context NAME | --server URL] [--client ID] [--terminal ID]\n\nlogout clears the selected server\'s saved login, including when offline.\nIts result reports whether remote revocation was confirmed; other independent logins are preserved."
        );
        return Ok(true);
    }
    options.server = super::connection_context::remote_server(
        options.context.as_deref(),
        options.server.as_deref(),
    )?;
    options.interactive |= interactive;
    if options.json && options.interactive {
        return Err("--json cannot be combined with interactive mode".into());
    }
    match operation {
        "login" => login(&options).await?,
        "logout" => logout_command(&options).await?,
        "whoami" => whoami(&options).await?,
        "list" | "ls" => list(&options).await?,
        _ => unreachable!(),
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn context_saved_create_values_cannot_select_remote_account_dispatch() {
        let args = |values: &[&str]| values.iter().map(OsString::from).collect::<Vec<_>>();
        for flag in ["--remote", "-i", "--interactive"] {
            assert!(
                remote_session_args(&args(&[
                    "sessions",
                    "create",
                    "build",
                    "--context",
                    "work",
                    "--title",
                    flag
                ]))
                .is_none()
            );
            assert!(remote_session_args(&args(&["sessions", "list", "--client", flag])).is_none());
        }
        for values in [
            vec!["sessions", "list", "--remote"],
            vec!["sessions", "--context", "work", "--remote"],
            vec!["sessions", "list", "--interactive"],
            vec!["sessions", "list", "--remote", "--help"],
        ] {
            assert!(remote_session_args(&args(&values)).is_some());
        }
        let literal_help = Arguments::parse(&args(&["--client", "--help"])).unwrap();
        assert!(!literal_help.help);
        assert_eq!(literal_help.client.as_deref(), Some("--help"));
    }
    #[test]
    fn interactive_help_explains_selection_and_the_local_detach_key() {
        let help = interactive_selection_help();
        assert!(help.contains("online session"));
        assert!(help.contains("existing remote terminal"));
        assert!(help.contains("Ctrl+C cancels selection"));
        assert!(help.contains("Ctrl+] returns to your local terminal"));
        let connected = interactive_connection_hint("session\u{1b}[31m", "main\n");
        assert!(!connected.chars().any(char::is_control));
        assert!(connected.contains("Ctrl+]"));
        assert!(connected.contains("remote session keeps running"));
    }
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
    fn login_link_never_carries_poll_secret_or_changes_origin() {
        let code = "a".repeat(64);
        assert!(
            verification_url(
                "https://example.com",
                "https://example.com/setup?code=ABCD",
                &code
            )
            .is_ok()
        );
        assert!(
            verification_url("https://example.com", "https://other.example/setup", &code).is_err()
        );
        assert!(
            verification_url(
                "https://example.com",
                &format!("https://example.com/setup?code={code}"),
                &code
            )
            .is_err()
        );
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
    fn selector_requires_a_unique_live_session() {
        let entries = vec![
            json!({"id":"one","name":"same"}),
            json!({"id":"two","name":"same"}),
        ];
        assert_eq!(select_client(&entries, "2").unwrap(), "two");
        assert_eq!(select_client(&entries, "one").unwrap(), "one");
        assert!(select_client(&entries, "same").is_err());
        assert!(select_client(&entries, "3").is_err());
    }
    async fn logout_fixture(
        root: &Path,
        status: StatusCode,
        renew_login: bool,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server = origin.clone();
        let root = root.to_owned();
        let app = axum::Router::new().route(
            "/api/client/logout",
            axum::routing::post(move |headers: axum::http::HeaderMap| {
                let root = root.clone();
                let server = server.clone();
                async move {
                    assert_eq!(
                        headers["authorization"].to_str().unwrap(),
                        format!("Bearer {}", "a".repeat(64))
                    );
                    assert!(!credential_path(&root, &server).exists());
                    if renew_login {
                        let mut replacement = credential(&server);
                        replacement.token = "b".repeat(64);
                        save_at(&root, &replacement).unwrap();
                    }
                    // A server error may reflect a secret. Logout's output must not.
                    (
                        status,
                        axum::Json(json!({"status":"logged_out","error":"a".repeat(64)})),
                    )
                }
            }),
        );
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (origin, task)
    }

    #[tokio::test]
    async fn logout_reports_remote_result_and_preserves_other_origins() {
        for (http, expected) in [
            (StatusCode::OK, LogoutStatus::Revoked),
            (StatusCode::UNAUTHORIZED, LogoutStatus::AlreadyInvalid),
            (StatusCode::FORBIDDEN, LogoutStatus::LocalOnly),
            (StatusCode::SERVICE_UNAVAILABLE, LogoutStatus::LocalOnly),
        ] {
            let root = temporary();
            let (server, task) = logout_fixture(&root, http, false).await;
            let mut selected = credential(&server);
            if http == StatusCode::UNAUTHORIZED {
                selected.expires_at = now() - 1;
            }
            save_at(&root, &selected).unwrap();
            let other = credential("https://other.example");
            save_at(&root, &other).unwrap();

            let outcome = logout_at(&root, &server, None).await.unwrap();
            assert_eq!(outcome.server, server);
            assert_eq!(outcome.status, expected);
            assert_eq!(outcome.json()["logged_out"], true);
            assert!(!outcome.message().contains(&selected.token));
            assert!(!outcome.json().to_string().contains(&selected.token));
            assert!(load_at(&root, Some(&server), true).unwrap().is_none());
            assert_eq!(
                load_at(&root, Some(&other.server), true)
                    .unwrap()
                    .unwrap()
                    .token,
                other.token
            );
            assert_eq!(selected_server(&root, None).unwrap(), other.server);
            task.abort();
            let _ = task.await;
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[tokio::test]
    async fn logout_offline_clears_local_login_and_is_idempotent() {
        let root = temporary();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        save_at(&root, &credential(&server)).unwrap();
        let outcome = logout_at(&root, &server, None).await.unwrap();
        assert_eq!(outcome.status, LogoutStatus::LocalOnly);
        assert!(
            outcome
                .message()
                .contains("Remote revocation could not be confirmed")
        );
        assert!(!credential_path(&root, &server).exists());
        assert_eq!(selected_server(&root, None).unwrap(), server);
        let repeated = logout_at(&root, &server, None).await.unwrap();
        assert_eq!(repeated.status, LogoutStatus::NotSignedIn);
        assert_eq!(repeated.json()["status"], "not_signed_in");
        assert_eq!(repeated.json()["logged_out"], true);
        let missing = root.join("missing");
        assert_eq!(
            logout_at(&missing, &server, None).await.unwrap().status,
            LogoutStatus::NotSignedIn
        );
        assert!(!missing.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn logout_preserves_a_new_login_saved_during_revocation() {
        let root = temporary();
        let (server, task) = logout_fixture(&root, StatusCode::OK, true).await;
        let active = credential(&server);
        save_at(&root, &active).unwrap();
        assert_eq!(
            logout_at(&root, &server, Some(&active.token))
                .await
                .unwrap()
                .status,
            LogoutStatus::Revoked
        );
        assert_eq!(
            load_at(&root, Some(&server), true).unwrap().unwrap().token,
            "b".repeat(64)
        );
        task.abort();
        let _ = task.await;
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn connection_logout_refuses_replaced_or_missing_saved_logins() {
        let root = temporary();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let server = format!("http://{}", listener.local_addr().unwrap());
        let active_token = "a".repeat(64);
        let mut replacement = credential(&server);
        replacement.token = "b".repeat(64);
        save_at(&root, &replacement).unwrap();
        let path = credential_path(&root, &server);
        let saved = fs::read(&path).unwrap();

        let error = logout_at(&root, &server, Some(&active_token))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("saved login changed"));
        assert!(error.contains("remote revocation was not attempted"));
        assert!(!error.contains(&active_token));
        assert!(!error.contains(&replacement.token));
        assert_eq!(fs::read(&path).unwrap(), saved);
        assert_eq!(selected_server(&root, None).unwrap(), server);
        assert!(
            matches!(listener.accept(), Err(error) if error.kind() == io::ErrorKind::WouldBlock)
        );

        fs::remove_file(&path).unwrap();
        let error = logout_at(&root, &server, Some(&active_token))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("no saved login for this connection"));
        assert!(error.contains("remote revocation was not attempted"));
        assert!(!path.exists());
        assert!(
            matches!(listener.accept(), Err(error) if error.kind() == io::ErrorKind::WouldBlock)
        );
        assert_eq!(
            logout_at(&root, &server, None).await.unwrap().status,
            LogoutStatus::NotSignedIn
        );
        fs::remove_dir_all(root).unwrap();
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
