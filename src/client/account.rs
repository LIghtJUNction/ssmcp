//! GitHub login and account-scoped control. Enrollment never saves an account
//! login on the controlled machine.
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
    net::IpAddr,
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::process::Command;

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

pub(super) fn normalize_server(raw: &str) -> Result<String> {
    if raw.bytes().any(|b| b.is_ascii_whitespace() || b == b'\\') {
        return Err("server must not contain whitespace or backslashes".into());
    }
    let url = Url::parse(raw).map_err(|_| "server must be an absolute HTTPS origin")?;
    let loopback = url.host_str().is_some_and(|host| {
        host.trim_matches(['[', ']'])
            .parse::<IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
    });
    if (url.scheme() != "https" && !(url.scheme() == "http" && loopback))
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !matches!(url.path(), "" | "/")
        || url.host_str().is_none()
        || url.port() == Some(0)
    {
        return Err("server must be an HTTPS origin without credentials, path, query or fragment; HTTP is allowed only for literal loopback addresses".into());
    }
    Ok(url.origin().ascii_serialization())
}

/// A named account destination always selects HTTPS; it never becomes an SSH alias.
pub(super) fn parse_target(raw: &str) -> Result<(String, String)> {
    let (username, host) = raw
        .split_once('@')
        .ok_or("use username@server, for example alice@ssmcp.lmm.best")?;
    let username = crate::username::normalize(username)?;
    if host.is_empty()
        || host.contains('@')
        || host.bytes().any(|b| b.is_ascii_whitespace() || b == b'\\')
    {
        return Err("invalid server in username@server".into());
    }
    let server = if host.contains("://") {
        normalize_server(host)?
    } else {
        if host.contains(['/', '?', '#', '%']) {
            return Err("server must be a hostname, IPv4 address or bracketed IPv6 address with an optional port".into());
        }
        normalize_server(&format!("https://{host}"))?
    };
    Ok((username, server))
}

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

fn read_private(path: &Path) -> Result<Option<Vec<u8>>> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
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
    if meta.len() > STORE_LIMIT as u64 {
        return Err("account credential exceeds its size limit".into());
    }
    let mut bytes = Vec::new();
    file.take(STORE_LIMIT as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > STORE_LIMIT {
        return Err("account credential exceeds its size limit".into());
    }
    Ok(Some(bytes))
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    if bytes.len() > STORE_LIMIT {
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
    root.join(format!("{:x}.json", Sha256::digest(server.as_bytes())))
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

/// Resolve only the selected origin; never borrow another server's credential.
pub(super) async fn credentials(server: Option<&str>) -> Result<Option<Credential>> {
    load_at(&state_root()?, server, false)
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
        Ok(Self {
            client: Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(5))
                .timeout(Duration::from_secs(15))
                .user_agent(concat!("ssmcp/", env!("CARGO_PKG_VERSION"), " client"))
                .build()?,
            server: normalize_server(server)?,
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
            .map_err(|_| "server request failed or timed out")?;
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
    json: bool,
    interactive: bool,
    no_browser: bool,
    name: Option<String>,
    client: Option<String>,
    terminal: Option<String>,
    ttl: Option<u64>,
    session_ttl: Option<u64>,
    no_bootstrap: bool,
    shell: Option<String>,
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
                Some("--server") => result.server = Some(normalize_server(&value()?)?),
                Some("--json") => result.json = true,
                Some("-i" | "--interactive") => result.interactive = true,
                Some("--remote") => {}
                Some("--no-browser") => result.no_browser = true,
                Some("--no-bootstrap") => result.no_bootstrap = true,
                Some("--shell") => {
                    let shell = value()?;
                    if !matches!(shell.as_str(), "sh" | "powershell") {
                        return Err("--shell must be sh or powershell".into());
                    }
                    result.shell = Some(shell);
                }
                Some("--name") => result.name = Some(value()?),
                Some("--client") => result.client = Some(value()?),
                Some("--terminal") => result.terminal = Some(value()?),
                Some("--ttl") => {
                    result.ttl = Some(value()?.parse().map_err(|_| "--ttl requires seconds")?)
                }
                Some("--session-ttl") => {
                    result.session_ttl = Some(
                        value()?
                            .parse()
                            .map_err(|_| "--session-ttl requires seconds")?,
                    )
                }
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

async fn open_browser(url: &Url) -> Result<()> {
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

async fn authorize_at(root: &Path, args: &Arguments, username: Option<&str>) -> Result<Credential> {
    let username = username.map(crate::username::normalize).transpose()?;
    let server = if root.exists() {
        verify_directory(&root)?;
        selected_server(&root, args.server.as_deref())?
    } else {
        normalize_server(args.server.as_deref().unwrap_or(DEFAULT_SERVER))?
    };
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
        .ok_or("invalid device login code")?;
    let raw_url = start["verification_uri_complete"]
        .as_str()
        .or_else(|| start["verification_uri"].as_str())
        .ok_or("server omitted the login URL")?;
    let url = verification_url(&server, raw_url, code)?;
    let expires = start["expires_in"]
        .as_u64()
        .filter(|n| (1..=1800).contains(n))
        .ok_or("invalid device login lifetime")?;
    let mut interval = start["interval"].as_u64().unwrap_or(2).clamp(1, 60);
    eprintln!("Open this link to sign in with GitHub:\n{url}");
    if let Some(user_code) = start["user_code"].as_str() {
        eprintln!("Confirmation code: {}", safe(user_code));
    }
    if !args.no_browser && open_browser(&url).await.is_err() {
        eprintln!("Open the printed link in your browser.");
    }
    let mut interrupt = platform::interrupt_signal()?;
    let mut terminate = platform::terminate_signal()?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(expires);
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

/// Obtain and validate the selected server's account without a separate login command.
pub(super) async fn connect_credential(
    server: Option<&str>,
    username: Option<&str>,
    no_browser: bool,
    allow_login: bool,
) -> Result<Credential> {
    connect_at(&state_root()?, server, username, no_browser, allow_login).await
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
        return Err("choose a username to connect: ssmcp username@ssmcp.lmm.best".into());
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

/// A legacy SSH ACL resolves a login name. Verify that it still refers to the
/// stable subject that authorized this CLI before forwarding that name.
pub(super) async fn verify_identity(credential: &Credential) -> Result<Credential> {
    let subject = credential.subject.as_deref()
        .ok_or("saved login has no stable GitHub identity; run ssmcp login again before using automatic SSH access")?;
    let api = Api::new(&credential.server)?;
    let reply = api
        .request(
            Method::GET,
            "/api/client/whoami?verify_github=true",
            Some(&credential.token),
            None,
        )
        .await?;
    if reply.status == StatusCode::CONFLICT {
        return Err("GitHub username ownership could not be verified; run ssmcp login again or retry when GitHub is available".into());
    }
    let identity = public_identity(&Api::checked(reply)?)?;
    if identity["subject"].as_str() != Some(subject) {
        return Err(
            "verified GitHub identity differs from the saved login; run ssmcp login again".into(),
        );
    }
    let mut verified = credential.clone();
    verified.principal = identity["principal"].as_str().unwrap().into();
    if verified.principal != credential.principal {
        save_at(&state_root()?, &verified)?;
    }
    Ok(verified)
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

async fn logout(args: &Arguments) -> Result<()> {
    let root = state_root()?;
    let Some(credential) = load_at(&root, args.server.as_deref(), true)? else {
        println!("Not signed in.");
        return Ok(());
    };
    let reply = Api::new(&credential.server)?
        .request(
            Method::POST,
            "/api/client/logout",
            Some(&credential.token),
            Some(json!({})),
        )
        .await?;
    if !reply.status.is_success() && reply.status != StatusCode::UNAUTHORIZED {
        Api::checked(reply)?;
    }
    fs::remove_file(credential_path(&root, &credential.server))?;
    if args.json {
        println!("{}", json!({"server":credential.server,"logged_out":true}));
    } else {
        println!("Signed out of {}.", credential.server);
    }
    Ok(())
}

pub(super) async fn enroll(server: &str, token: &str) -> Result<Credential> {
    if !valid_secret(token) {
        return Err("invalid temporary enrollment token".into());
    }
    let api = Api::new(server)?;
    let reply = api
        .request(
            Method::POST,
            "/api/client/enroll",
            None,
            Some(json!({"token":token})),
        )
        .await?;
    if matches!(
        reply.status,
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN | StatusCode::CONFLICT | StatusCode::GONE
    ) {
        return Err("temporary command expired, was already used, or was revoked; generate a new command on device A".into());
    }
    let value = Api::checked(reply)?;
    let principal = crate::wire::canonical_github_principal(
        value["principal"]
            .as_str()
            .ok_or("enrollment omitted its owner")?,
    )?;
    let token = value["device_token"]
        .as_str()
        .filter(|v| valid_secret(v))
        .ok_or("invalid device token")?;
    let expires = value["expires_in"]
        .as_u64()
        .filter(|n| (1..=86400 * 30).contains(n))
        .ok_or("invalid temporary session lifetime")?;
    Ok(Credential {
        server: api.server,
        token: token.into(),
        principal,
        subject: None,
        username: None,
        expires_at: now().saturating_add(expires),
    })
}

fn shell_quote(value: &str, powershell: bool) -> String {
    if powershell {
        format!("'{}'", value.replace('\'', "''"))
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

fn join_command(server: &str, token: &str) -> Result<String> {
    command(server, token, false, false)
}

fn command(server: &str, token: &str, bootstrap: bool, powershell: bool) -> Result<String> {
    let server = normalize_server(server)?;
    if !valid_secret(token) {
        return Err("server returned an invalid enrollment token".into());
    }
    let quoted_server = shell_quote(&server, powershell);
    let quoted_token = shell_quote(token, powershell);
    if !bootstrap {
        return Ok(format!(
            "ssmcp join --server {quoted_server} --token {quoted_token}"
        ));
    }
    if powershell {
        let script = shell_quote(&format!("{server}/client/install.ps1"), true);
        Ok(format!(
            "& ([scriptblock]::Create((Invoke-WebRequest -UseBasicParsing -MaximumRedirection 0 -ErrorAction Stop {script}).Content)) -Server {quoted_server} -Token {quoted_token}"
        ))
    } else {
        let script = shell_quote(&format!("{server}/client/install.sh"), false);
        Ok(format!(
            r#"(ssmcp_bootstrap=$(if command -v curl >/dev/null 2>&1; then curl -fsSL --max-redirs 0 -- {script}; else wget -qO- --tries=1 --max-redirect=0 -- {script}; fi) && test -n "$ssmcp_bootstrap" && sh -c "$ssmcp_bootstrap" sh --server {quoted_server} --token {quoted_token})"#
        ))
    }
}

async fn invite(args: &Arguments) -> Result<()> {
    let ttl = args.ttl.unwrap_or(600);
    let session_ttl = args.session_ttl.unwrap_or(3600);
    if !(30..=3600).contains(&ttl) || !(60..=86400).contains(&session_ttl) {
        return Err(
            "--ttl must be 30..=3600 seconds; --session-ttl must be 60..=86400 seconds".into(),
        );
    }
    let credential = require_credential(args.server.as_deref()).await?;
    let value = Api::new(&credential.server)?
        .post(
            "/api/client/invites",
            Some(&credential.token),
            json!({"ttl_secs":ttl,"session_ttl_secs":session_ttl}),
        )
        .await?;
    let token = value["token"]
        .as_str()
        .ok_or("server omitted the enrollment token")?;
    let sh = if args.no_bootstrap {
        join_command(&credential.server, token)?
    } else {
        command(&credential.server, token, true, false)?
    };
    let powershell = command(&credential.server, token, !args.no_bootstrap, true)?;
    let selected =
        if args
            .shell
            .as_deref()
            .unwrap_or(if cfg!(windows) { "powershell" } else { "sh" })
            == "powershell"
        {
            &powershell
        } else {
            &sh
        };
    if args.json {
        println!(
            "{}",
            json!({"command":selected,"command_sh":sh,"command_powershell":powershell,
            "expires_in":value["expires_in"],"session_expires_in":value["session_expires_in"]})
        );
    } else {
        println!("{selected}");
        eprintln!(
            "Run this once on device B. Claim within {} seconds; session access lasts {} seconds.",
            value["expires_in"].as_u64().unwrap_or(ttl),
            value["session_expires_in"].as_u64().unwrap_or(session_ttl)
        );
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
async fn prompt_selection() -> Result<String> {
    eprint!("Session number, ID or name: ");
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
        if args.json {
            println!("{}", serde_json::to_string_pretty(&value)?);
        } else {
            print!("{}", list_table(entries));
        }
        return Ok(());
    }
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(
            "interactive mode requires a terminal; use ssmcp list --json for scripts".into(),
        );
    }
    if entries.is_empty() {
        print!("{}", list_table(entries));
        return Ok(());
    }
    let selected = if let Some(id) = &args.client {
        id.clone()
    } else {
        eprint!("{}", list_table(entries));
        prompt_selection().await?
    };
    let id = select_client(entries, &selected)?;
    interactive(
        &api,
        &credential,
        id,
        args.terminal.as_deref().unwrap_or("main"),
    )
    .await
}

struct Reader(tokio::task::JoinHandle<()>);
impl Drop for Reader {
    fn drop(&mut self) {
        self.0.abort();
    }
}
async fn interactive(api: &Api, credential: &Credential, id: &str, terminal: &str) -> Result<()> {
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
    let mut keyboard = input::Keyboard::open()?;
    eprintln!(
        "Connected to {} / {} · Ctrl-] detaches · Ctrl-C reaches the remote shell",
        safe(id),
        safe(terminal)
    );
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
                    input::Event::Detach|input::Event::End=>break,
                    input::Event::Text(text)=>{
                        api.call(credential,id,"terminal.write",json!({"terminal_id":terminal,"data":text})).await?;
                    }
                    input::Event::Resize(rows,cols)=>{
                        api.call(credential,id,"terminal.resize",json!({"terminal_id":terminal,"rows":rows.clamp(1,1000),"cols":cols.clamp(1,1000)})).await?;
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
            Some("--server" | "--client" | "--terminal") => {
                if args.get(index + 1).is_none() {
                    return false;
                }
                index += 2;
            }
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

/// Account commands run before legacy parsing; a valued -i keeps SSH semantics.
pub(super) async fn dispatch(args: &[OsString]) -> Result<bool> {
    let Some(first) = args.first().and_then(|v| v.to_str()) else {
        return Ok(false);
    };
    let (operation, rest, interactive) = match first {
        "login" | "logout" | "whoami" | "list" | "ls" | "invite" => (first, &args[1..], false),
        "sessions"
            if args
                .iter()
                .any(|a| a == "--remote" || a == "-i" || a == "--interactive") =>
        {
            let rest = if args.get(1).is_some_and(|a| a == "list") {
                &args[2..]
            } else {
                &args[1..]
            };
            ("list", rest, false)
        }
        _ if standalone_interactive(args) => ("list", args, true),
        _ => return Ok(false),
    };
    if rest.iter().any(|a| a == "--help" || a == "-h") {
        println!(
            "ssmcp login [--server URL] [--no-browser] [--name NAME]\nssmcp whoami|logout [--server URL] [--json]\nssmcp list [--server URL] [--json | -i] [--client ID] [--terminal ID]\nssmcp -i [--server URL] [--client ID] [--terminal ID]\nssmcp invite [--server URL] [--ttl SECONDS] [--session-ttl SECONDS] [--shell sh|powershell] [--no-bootstrap] [--json]"
        );
        return Ok(true);
    }
    let mut options = Arguments::parse(rest)?;
    options.interactive |= interactive;
    if options.json && options.interactive {
        return Err("--json cannot be combined with interactive mode".into());
    }
    match operation {
        "login" => login(&options).await?,
        "logout" => logout(&options).await?,
        "whoami" => whoami(&options).await?,
        "list" | "ls" => list(&options).await?,
        "invite" => invite(&options).await?,
        _ => unreachable!(),
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn temporary() -> PathBuf {
        let mut random = [0; 16];
        platform::random_bytes(&mut random).unwrap();
        let root = std::env::temp_dir().join(format!("ssmcp-auth-{:x}", Sha256::digest(random)));
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
    fn invite_command_cannot_inject_shell_syntax() {
        let token = "b".repeat(64);
        assert_eq!(
            join_command("https://example.com", &token).unwrap(),
            format!("ssmcp join --server 'https://example.com' --token '{token}'")
        );
        assert!(join_command("https://example.com' ; touch /tmp/x", &token).is_err());
        assert!(join_command("https://example.com", "$(whoami)").is_err());
        assert!(join_command("https://example.com", &(token + "'")).is_err());
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
    #[cfg(unix)]
    #[test]
    fn posix_quoting_preserves_arguments_without_executing_their_content() {
        let value = "example'$(printf WRONG);\\\\value";
        let output = std::process::Command::new("sh")
            .args(["-c", &format!("printf %s {}", shell_quote(value, false))])
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(String::from_utf8(output.stdout).unwrap(), value);
        let bootstrap = command("https://example.com", &"c".repeat(64), true, false).unwrap();
        assert!(
            std::process::Command::new("sh")
                .args(["-n", "-c", &bootstrap])
                .status()
                .unwrap()
                .success()
        );
        assert!(bootstrap.contains("/client/install.sh"));
        assert!(bootstrap.contains("command -v curl"));
        assert!(bootstrap.contains("wget -qO-"));
        let powershell = command("https://example.com", &"c".repeat(64), true, true).unwrap();
        assert!(powershell.contains("/client/install.ps1"));
        assert!(powershell.contains("-MaximumRedirection 0"));
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
