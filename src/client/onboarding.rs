//! Interactive first-connection setup. Device authorization never leaves the trusted portal.
mod dependencies;
#[cfg(all(test, unix))]
mod flow_tests;
mod probe_process;
mod ssh_config;

use super::{Options, render};
use crate::backend::process::Managed;
use base64::{
    Engine,
    engine::general_purpose::{STANDARD, STANDARD_NO_PAD},
};
use reqwest::{Client, Url};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    ffi::OsString,
    io::{self, IsTerminal, Write},
    net::IpAddr,
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};
use tokio::{io::AsyncReadExt, process::Command};

const API_LIMIT: usize = 64 * 1024;
const SETUP_TTL: Duration = Duration::from_secs(600);

pub(super) fn run_askpass_helper() -> Option<i32> {
    probe_process::run_askpass_helper()
}

pub(super) fn interactive() -> bool {
    io::stdin().is_terminal() && io::stderr().is_terminal()
}

pub(super) fn ssh_executable() -> OsString {
    dependencies::ssh_executable()
}

fn safe_line(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).take(512).collect()
}

fn protocol_label(value: &str) -> String {
    let mut label = String::new();
    for ch in value.chars().filter(|c| !c.is_control()) {
        if label.len() + ch.len_utf8() > 80 {
            break;
        }
        label.push(ch);
    }
    let label = label.trim();
    if label.is_empty() {
        "computer-use".into()
    } else {
        label.into()
    }
}

fn portal_url(raw: &str) -> Result<Url, String> {
    let mut url = Url::parse(raw).map_err(|_| "Portal must be an absolute HTTPS URL.")?;
    let local = url.host_str().is_some_and(|host| {
        host.trim_matches(['[', ']'])
            .parse::<IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
    });
    if (url.scheme() != "https" && !(url.scheme() == "http" && local))
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !matches!(url.path(), "" | "/")
        || url.host_str().is_none()
    {
        return Err("Portal must be an HTTPS origin without credentials, path, query or fragment; HTTP is allowed only for literal loopback test addresses.".into());
    }
    url.set_path("/");
    Ok(url)
}

fn verification_url(base: &Url, raw: &str, code: &str) -> Result<Url, String> {
    let url = Url::parse(raw).map_err(|_| "Server returned an invalid verification URL.")?;
    if url.origin() != base.origin()
        || url.path() != "/setup"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err("The verification URL does not belong to the selected portal.".into());
    }
    let pairs: Vec<_> = url.query_pairs().collect();
    if pairs.len() != 1 || pairs[0].0 != "code" || pairs[0].1 != code {
        return Err("The verification URL does not match this setup request.".into());
    }
    Ok(url)
}

struct ApiReply {
    status: reqwest::StatusCode,
    retry_after: Duration,
    value: Value,
}

struct Api {
    client: Client,
    base: Url,
}
impl Api {
    fn new(base: Url) -> Result<Self, String> {
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(12))
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("ssmcp/", env!("CARGO_PKG_VERSION"), " setup"))
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Self { client, base })
    }
    async fn request(&self, route: &str, body: Value) -> Result<ApiReply, String> {
        let url = self.base.join(route).map_err(|e| e.to_string())?;
        let mut response = self
            .client
            .post(url)
            .json(&body)
            .send()
            .await
            .map_err(|error| {
                use std::error::Error;
                let mut details = error.to_string();
                let mut source = error.source();
                while let Some(cause) = source {
                    details.push_str(": ");
                    details.push_str(&cause.to_string());
                    source = cause.source();
                }
                format!(
                    "Portal request to {} failed: {}. Check --portal and network connectivity.",
                    self.base,
                    safe_line(&details)
                )
            })?;
        let status = response.status();
        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(2)
            .clamp(1, 60);
        if status.is_redirection() {
            return Err("Portal API redirected the request; check --portal. Authorization was not forwarded.".into());
        }
        if response
            .content_length()
            .is_some_and(|n| n > API_LIMIT as u64)
        {
            return Err("Portal response exceeds 64 KiB.".into());
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| "Portal response was interrupted.")?
        {
            if bytes.len().saturating_add(chunk.len()) > API_LIMIT {
                return Err("Portal response exceeds 64 KiB.".into());
            }
            bytes.extend_from_slice(&chunk);
        }
        let value: Value = serde_json::from_slice(&bytes).map_err(|_| {
            if status == reqwest::StatusCode::NOT_FOUND {
                "This server has not enabled CLI setup; update ssmcpd or use its existing account portal."
            } else { "Portal returned an invalid JSON response." }
        })?;
        Ok(ApiReply {
            status,
            retry_after: Duration::from_secs(retry_after),
            value,
        })
    }
    fn checked(reply: ApiReply) -> Result<Value, String> {
        if !reply.status.is_success() {
            let error = reply.value["error"].as_str().unwrap_or("request_failed");
            let message = reply.value["message"]
                .as_str()
                .unwrap_or("Check the portal and retry.");
            return Err(format!(
                "Portal {}: {} ({})",
                safe_line(error),
                safe_line(message),
                reply.status
            ));
        }
        Ok(reply.value)
    }
    async fn post(&self, route: &str, body: Value) -> Result<Value, String> {
        Self::checked(self.request(route, body).await?)
    }
    async fn cancel(&self, secret: &str) {
        let _ = self
            .post("api/setup/cancel", json!({"device_code":secret}))
            .await;
    }
}

#[derive(Deserialize)]
struct Start {
    device_code: String,
    user_code: String,
    verification_uri_complete: String,
    expires_in: u64,
    interval: u64,
}
#[derive(Clone, Deserialize)]
struct AccountKey {
    public_key: String,
    fingerprint: String,
}
#[derive(Clone, Deserialize)]
struct Account {
    github_login: String,
    username: String,
    keys: Vec<AccountKey>,
}
impl Account {
    fn validate(&self) -> Result<(), String> {
        crate::wire::canonical_github_principal(&format!("{}@github", self.github_login))?;
        if self.username.is_empty()
            || self.username.len() > 64
            || !self
                .username
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
            || self.keys.len() > 20
        {
            return Err("Portal returned an invalid SSH account.".into());
        }
        for key in &self.keys {
            let (_, fingerprint) = parse_public_key(&key.public_key)?;
            if fingerprint != key.fingerprint {
                return Err("Portal public key fingerprint does not match its key.".into());
            }
        }
        Ok(())
    }
}
struct Target {
    host: String,
    port: u16,
    portal: Url,
    config: PathBuf,
}

fn inferred_portal_host<'a>(requested: &'a str, resolved: &'a str) -> &'a str {
    // A DNS destination is also the HTTPS identity. SSH HostName may replace
    // it with an address for routing, which must not replace TLS SNI.
    let requested = requested.trim_matches(['[', ']']);
    if requested.contains('.') && requested.parse::<IpAddr>().is_err() {
        requested
    } else {
        resolved
    }
}

async fn bounded_output(
    program: &Path,
    args: &[OsString],
    seconds: u64,
    limit: usize,
) -> Result<Vec<u8>, String> {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut process = Managed::spawn(&mut command)
        .map_err(|e| format!("Could not start {}: {e}", program.display()))?;
    let mut stdout = process
        .child
        .stdout
        .take()
        .ok_or("Missing stdout")?
        .take((limit + 1) as u64);
    let mut stderr = process
        .child
        .stderr
        .take()
        .ok_or("Missing stderr")?
        .take(4097);
    let mut output = Vec::new();
    let mut error = Vec::new();
    let result = tokio::time::timeout(Duration::from_secs(seconds), async {
        let (out, err, status) = tokio::join!(
            stdout.read_to_end(&mut output),
            stderr.read_to_end(&mut error),
            process.wait()
        );
        out.map_err(|e| e.to_string())?;
        err.map_err(|e| e.to_string())?;
        let status = status.map_err(|e| e.to_string())?;
        if output.len() > limit || error.len() > 4096 {
            return Err("SSH helper exceeded its output budget.".into());
        }
        if !status.success() {
            return Err(format!(
                "SSH helper failed ({status}): {}",
                safe_line(&String::from_utf8_lossy(&error))
            ));
        }
        Ok(output)
    })
    .await
    .map_err(|_| format!("SSH helper exceeded its {seconds}-second deadline."))
    .and_then(|v| v);
    process.shutdown().await;
    result
}

fn ssh_g_args(options: &Options, destination: &str) -> Vec<OsString> {
    let mut args = vec!["-G".into(), "-o".into(), "CanonicalizeHostname=no".into()];
    if let Some(path) = &options.config
        && Path::new(path).exists()
    {
        args.extend(["-F".into(), path.clone()]);
    }
    if let Some(port) = options.port {
        args.extend(["-p".into(), port.to_string().into()]);
    }
    args.extend(["--".into(), destination.into()]);
    args
}

fn effective_fields(text: &str) -> Result<std::collections::BTreeMap<String, Vec<String>>, String> {
    let mut values = std::collections::BTreeMap::<String, Vec<String>>::new();
    for line in text.lines() {
        if let Some((key, value)) = line.split_once(' ') {
            values
                .entry(key.to_owned())
                .or_default()
                .push(value.trim().to_owned());
        }
    }
    if !values.contains_key("hostname")
        || !values.contains_key("port")
        || !values.contains_key("user")
    {
        return Err("OpenSSH did not return a complete effective configuration.".into());
    }
    Ok(values)
}

impl Target {
    async fn resolve(options: &Options, home: &Path) -> Result<Self, String> {
        let destination = options
            .destination
            .as_deref()
            .ok_or("Missing SSH destination.")?;
        let raw_host = destination
            .rsplit_once('@')
            .map_or(destination, |(_, host)| host);
        let mut host = raw_host.trim_matches(['[', ']']).to_owned();
        let mut port = options.port.unwrap_or(22);
        if let Ok(programs) = dependencies::discover()
            && options
                .config
                .as_ref()
                .is_none_or(|path| Path::new(path).exists())
        {
            let output = bounded_output(
                &programs.ssh,
                &ssh_g_args(options, destination),
                5,
                API_LIMIT,
            )
            .await?;
            let text =
                std::str::from_utf8(&output).map_err(|_| "OpenSSH config output is not UTF-8.")?;
            let values = effective_fields(text)?;
            host = values["hostname"]
                .first()
                .ok_or("Missing hostname")?
                .trim_matches(['[', ']'])
                .to_owned();
            port = values["port"]
                .first()
                .and_then(|v| v.parse().ok())
                .filter(|p| *p != 0)
                .ok_or("Invalid SSH port")?;
        }
        if host.is_empty()
            || host.len() > 253
            || host.starts_with('-')
            || !host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b".:_-".contains(&b))
        {
            return Err(
                "Could not resolve a safe SSH hostname; use --portal for a custom account origin."
                    .into(),
            );
        }
        let portal_host = inferred_portal_host(raw_host, &host);
        let inferred = if portal_host.contains(':') {
            format!("https://[{portal_host}]")
        } else {
            format!("https://{portal_host}")
        };
        let portal = portal_url(options.portal.as_deref().unwrap_or(&inferred))?;
        let config = options
            .config
            .as_ref()
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".ssh/config"));
        let config = if config.is_absolute() {
            config
        } else {
            std::env::current_dir()
                .map_err(|e| e.to_string())?
                .join(config)
        };
        Ok(Self {
            host,
            port,
            portal,
            config,
        })
    }
}

trait Console {
    async fn line(&mut self, prompt: &str) -> Result<String, String>;
    async fn open(&mut self, url: &str) -> Result<(), String>;
    fn say(&mut self, message: &str);
}
struct Terminal;
impl Console for Terminal {
    async fn line(&mut self, prompt: &str) -> Result<String, String> {
        eprint!("{prompt}");
        io::stderr().flush().map_err(|e| e.to_string())?;
        let (sender, receiver) = tokio::sync::oneshot::channel();
        // An ordinary detached thread does not keep Tokio shutdown waiting on a blocked stdin.
        std::thread::spawn(move || {
            use std::io::{BufRead, Read};
            let input = io::stdin();
            let mut input = input.lock().take(4097);
            let mut line = String::new();
            let result = input
                .read_line(&mut line)
                .map_err(|e| e.to_string())
                .and_then(|n| {
                    if n == 0 {
                        Err("Input ended; setup was cancelled.".into())
                    } else if line.len() > 4096 {
                        Err("Input exceeds 4096 bytes.".into())
                    } else {
                        Ok(line.trim_end_matches(['\r', '\n']).to_owned())
                    }
                });
            let _ = sender.send(result);
        });
        receiver
            .await
            .map_err(|_| "Could not read terminal input.".to_owned())?
    }
    async fn open(&mut self, url: &str) -> Result<(), String> {
        let mut command = if cfg!(windows) {
            let mut c = Command::new("rundll32.exe");
            c.args(["url.dll,FileProtocolHandler", url]);
            c
        } else if cfg!(target_os = "macos") {
            let mut c = Command::new("/usr/bin/open");
            c.arg(url);
            c
        } else {
            let mut c = Command::new("xdg-open");
            c.arg(url);
            c
        };
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = command.spawn().map_err(
            |_| "Could not open a browser automatically; open the printed URL manually.",
        )?;
        // The browser owns its lifetime; never kill it at the setup timeout.
        tokio::spawn(async move {
            let _ = child.wait().await;
        });
        Ok(())
    }
    fn say(&mut self, message: &str) {
        eprintln!("{message}");
    }
}

fn yes(value: &str) -> bool {
    matches!(value.trim().to_ascii_lowercase().as_str(), "yes" | "y")
}

fn parse_public_key(raw: &str) -> Result<(String, String), String> {
    if raw.len() > 4096 || raw.contains('\0') || raw.trim().lines().count() != 1 {
        return Err("Expected one Ed25519 public key line.".into());
    }
    let mut fields = raw.split_whitespace();
    if fields.next() != Some("ssh-ed25519") {
        return Err("Setup requires an ssh-ed25519 public key.".into());
    }
    let encoded = fields.next().ok_or("Public key is missing its data")?;
    let bytes = STANDARD
        .decode(encoded)
        .map_err(|_| "Invalid public key base64")?;
    if bytes.len() != 51 || &bytes[..19] != b"\0\0\0\x0bssh-ed25519\0\0\0\x20" {
        return Err("Invalid Ed25519 public key structure.".into());
    }
    let fingerprint = format!("SHA256:{}", STANDARD_NO_PAD.encode(Sha256::digest(&bytes)));
    Ok((
        format!("ssh-ed25519 {}", STANDARD.encode(bytes)),
        fingerprint,
    ))
}

fn public_file(path: &Path) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(".pub");
    value.into()
}

fn read_public(identity: &Path) -> Result<(String, String), String> {
    let private =
        std::fs::metadata(identity).map_err(|_| "Private key file is not present locally.")?;
    if !private.is_file() {
        return Err("Private key is not a regular file.".into());
    }
    let path = public_file(identity);
    let meta = std::fs::metadata(&path).map_err(|_| "Public key file is not present locally.")?;
    if !meta.is_file() || meta.len() > 4096 {
        return Err("Public key file exceeds its size limit.".into());
    }
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    parse_public_key(&text)
}

fn managed_key(home: &Path, target: &Target) -> PathBuf {
    home.join(".ssh/ssmcp").join(format!(
        "id_ed25519_{}",
        ssh_config::alias(&target.host, target.port)
    ))
}

async fn generate_key(programs: &dependencies::SshPrograms, path: &Path) -> Result<(), String> {
    if std::fs::symlink_metadata(path).is_ok()
        || std::fs::symlink_metadata(public_file(path)).is_ok()
    {
        return Err("The dedicated key path already exists; no key was overwritten.".into());
    }
    let parent = path.parent().ok_or("Key path has no parent")?;
    ssh_config::ensure_private_directory(parent)?;
    let args = vec![
        "-q".into(),
        "-t".into(),
        "ed25519".into(),
        "-N".into(),
        "".into(),
        "-C".into(),
        "ssmcp computer-use".into(),
        "-f".into(),
        path.as_os_str().into(),
    ];
    bounded_output(&programs.keygen, &args, 15, 4096).await?;
    read_public(path)?;
    Ok(())
}

async fn verified_config(
    programs: &dependencies::SshPrograms,
    path: &Path,
    alias: &str,
    target: &Target,
    user: &str,
    identity: &Path,
) -> Result<(), String> {
    let args = vec![
        "-G".into(),
        "-F".into(),
        path.as_os_str().into(),
        "-o".into(),
        "CanonicalizeHostname=no".into(),
        "--".into(),
        alias.into(),
    ];
    let bytes = bounded_output(&programs.ssh, &args, 5, API_LIMIT).await?;
    let text = std::str::from_utf8(&bytes).map_err(|_| "Effective SSH config is not UTF-8")?;
    let values = effective_fields(text)?;
    let exact = |key: &str, value: &str| {
        values
            .get(key)
            .and_then(|v| v.first())
            .is_some_and(|v| v == value)
    };
    if !exact("hostname", &target.host)
        || !exact("user", user)
        || !exact("port", &target.port.to_string())
        || !exact("identitiesonly", "yes")
    {
        return Err("OpenSSH effective settings do not match the saved account; existing configuration was preserved in its backup.".into());
    }
    let identity = identity.to_string_lossy().replace('\\', "/");
    if !values.get("identityfile").is_some_and(|all| {
        all.iter()
            .any(|v| v.replace('\\', "/").replace("%%", "%") == identity)
    }) {
        return Err("OpenSSH did not select the dedicated identity file.".into());
    }
    Ok(())
}

async fn wait_authorized<C: Console>(
    api: &Api,
    start: &Start,
    console: &mut C,
) -> Result<Account, String> {
    let deadline =
        Instant::now() + Duration::from_secs(start.expires_in.clamp(1, SETUP_TTL.as_secs()));
    let interval = Duration::from_secs(start.interval.clamp(1, 5));
    loop {
        if Instant::now() >= deadline {
            return Err("Browser authorization expired; run setup again.".into());
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        let reply = tokio::time::timeout(
            remaining,
            api.request("api/setup/poll", json!({"device_code":start.device_code})),
        )
        .await
        .map_err(|_| "Browser authorization expired; run setup again.")??;
        if reply.status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            tokio::time::sleep(
                reply
                    .retry_after
                    .min(deadline.saturating_duration_since(Instant::now())),
            )
            .await;
            continue;
        }
        let response = Api::checked(reply)?;
        match response["status"].as_str() {
            Some("pending") => {
                tokio::time::sleep(interval.min(deadline.saturating_duration_since(Instant::now())))
                    .await
            }
            Some("authorized") => {
                let account: Account = serde_json::from_value(response["account"].clone())
                    .map_err(|_| "Portal returned an incomplete account.")?;
                account.validate()?;
                console.say(&format!(
                    "已登录 GitHub：{}；账号已有 {} 把公钥。",
                    account.github_login,
                    account.keys.len()
                ));
                return Ok(account);
            }
            Some("denied") => {
                return Err(
                    "You declined browser authorization; no key or SSH config was changed.".into(),
                );
            }
            Some("expired") => return Err("Browser authorization expired; run setup again.".into()),
            _ => return Err("Portal returned an unknown authorization state.".into()),
        }
    }
}

async fn complete<C: Console>(
    options: &mut Options,
    home: &Path,
    target: &Target,
    api: &Api,
    start: &Start,
    mut account: Account,
    console: &mut C,
) -> Result<(), String> {
    let original_account = (
        account.username.clone(),
        account.github_login.to_ascii_lowercase(),
    );
    let programs = match dependencies::discover() {
        Ok(programs) => programs,
        Err(reason) => {
            console.say(&format!(
                "本机缺少可用的 SSH 客户端或 ssh-keygen：{}",
                safe_line(&reason)
            ));
            if !yes(&console.line("是否安装所需 SSH 客户端？[yes/NO] ").await?) {
                return Err("SSH installation was declined; no public key was submitted.".into());
            }
            console.say("正在补齐 SSH。下载过慢会停止并尝试已配置的加速源；安装前验证文件。");
            dependencies::install().await?
        }
    };
    let ssh_dir = home.join(".ssh");
    ssh_config::ensure_private_directory(&ssh_dir)?;
    let managed = managed_key(home, target);
    let mut candidates = Vec::new();
    if let Some(explicit) = &options.identity {
        let p = PathBuf::from(explicit);
        candidates.push(if p.is_absolute() {
            p
        } else {
            std::env::current_dir().map_err(|e| e.to_string())?.join(p)
        });
    }
    if options.identity.is_none() {
        candidates.push(managed.clone());
        candidates.push(ssh_dir.join("id_ed25519"));
    }
    let mut available = Vec::new();
    for path in candidates {
        let (public, fingerprint) = match read_public(&path) {
            Ok(pair) => pair,
            Err(error) if options.identity.is_some() => {
                return Err(format!(
                    "Cannot use the explicitly selected -i identity {}: {}. Fix this path and its Ed25519 .pub file, or omit -i to choose a setup key.",
                    safe_line(&path.display().to_string()),
                    safe_line(&error),
                ));
            }
            Err(_) => continue,
        };
        if !available
            .iter()
            .any(|(p, _, _): &(PathBuf, String, String)| p == &path)
        {
            available.push((path, public, fingerprint));
        }
    }
    let registered = available
        .iter()
        .find(|(_, public, fingerprint)| {
            account
                .keys
                .iter()
                .any(|k| k.public_key == *public && k.fingerprint == *fingerprint)
        })
        .cloned();
    let identity = if let Some((path, _, fingerprint)) = registered {
        console.say(&format!("已找到账号登记过的本机密钥：{fingerprint}"));
        path
    } else {
        let (path, public, fingerprint) = if let Some(existing) = available.first().cloned() {
            console.say(&format!("找到本机 Ed25519 公钥：{}", existing.2));
            if !yes(&console
                .line("是否将这把公钥登记到当前 GitHub 身份对应的 ssmcp 账户？[yes/NO] ")
                .await?)
            {
                return Err("Public key registration was declined.".into());
            }
            existing
        } else {
            if !yes(&console.line("是否创建 ssmcp 专用 Ed25519 密钥并提交公钥？密钥不设口令以便后台重连，私钥仅保留在本机。[yes/NO] ").await?) {
                return Err("Key creation was declined; no key or SSH config was changed.".into());
            }
            ssh_config::ensure_private_directory(
                managed.parent().ok_or("Missing managed key directory")?,
            )?;
            generate_key(&programs, &managed).await?;
            let (public, fingerprint) = read_public(&managed)?;
            (managed, public, fingerprint)
        };
        let label = protocol_label(&options.name.clone().unwrap_or_else(|| {
            crate::platform::hostname().unwrap_or_else(|_| "computer-use".into())
        }));
        let response = api
            .post(
                "api/setup/key",
                json!({"device_code":start.device_code,"public_key":public,"label":label}),
            )
            .await?;
        if response["status"] != "saved" {
            return Err("The server did not confirm public key registration.".into());
        }
        account = serde_json::from_value(response["account"].clone())
            .map_err(|_| "Missing account after key registration.")?;
        account.validate()?;
        if account.username != original_account.0
            || account.github_login.to_ascii_lowercase() != original_account.1
        {
            return Err("Portal changed the authorized account during key registration; SSH config was not written.".into());
        }
        if !account
            .keys
            .iter()
            .any(|k| k.fingerprint == fingerprint && k.public_key == public)
        {
            return Err("The registered key is absent from the verified account response.".into());
        }
        console.say(&format!("公钥已登记：{fingerprint}"));
        path
    };
    let default_name = options.name.clone().unwrap_or_else(|| {
        let hostname = crate::platform::hostname().unwrap_or_else(|_| "computer-use".into());
        let name: String = hostname
            .chars()
            .filter(|c| !c.is_control())
            .take(32)
            .collect();
        if name.trim().is_empty() {
            "computer-use".into()
        } else {
            name
        }
    });
    let name = loop {
        let entered = console
            .line(&format!("给这台电脑取个名字 [{default_name}]："))
            .await?;
        let name = if entered.trim().is_empty() {
            default_name.clone()
        } else {
            entered.trim().to_owned()
        };
        if !name.is_empty() && name.len() <= 128 && !name.chars().any(char::is_control) {
            break name;
        }
        console.say(
            "名称需要 1–128 个 UTF-8 字节，且不能包含控制字符。请重新输入，或回车使用默认名称。",
        );
    };
    let saved = ssh_config::save(
        &target.config,
        &target.host,
        target.port,
        &account.username,
        &identity,
    )?;
    verified_config(
        &programs,
        &saved.path,
        &saved.alias,
        target,
        &account.username,
        &identity,
    )
    .await?;
    options.destination = Some(saved.alias.clone());
    options.name = Some(name);
    options.identity = Some(identity.into_os_string());
    options.config = Some(saved.path.as_os_str().into());
    options.port = Some(target.port);
    if options.allow.is_empty() {
        options
            .allow
            .push(crate::wire::canonical_github_principal(&format!(
                "{}@github",
                account.github_login
            ))?);
    }
    console.say(&format!(
        "SSH 配置已{}并通过 ssh -G 检查：{}",
        if saved.changed { "保存" } else { "确认" },
        saved.path.display()
    ));
    if let Some(backup) = saved.backup {
        console.say(&format!("原配置备份：{}", backup.display()));
    }
    console.say(&format!(
        "电脑名称：{}；连接别名：{}。正在检查连接，首次主机密钥确认仍由 OpenSSH 完成。",
        options.name.as_deref().unwrap_or("computer-use"),
        saved.alias
    ));
    Ok(())
}

pub(super) async fn run(options: &mut Options) -> Result<bool, String> {
    if !interactive() {
        return Err("Interactive setup needs a terminal. Run ssmcp setup HOST before starting a background service, or provision SSH explicitly.".into());
    }
    let mut interrupt = crate::platform::interrupt_signal().map_err(|e| e.to_string())?;
    let mut terminate = crate::platform::terminate_signal().map_err(|e| e.to_string())?;
    let prepared = async {
        let home = ssh_config::home()?;
        let target = Target::resolve(options, &home).await?;
        let api = Api::new(target.portal.clone())?;
        let client_name = protocol_label(&options.name.clone().unwrap_or_else(|| {
            crate::platform::hostname().unwrap_or_else(|_| "computer-use".into())
        }));
        let start: Start = serde_json::from_value(
            api.post("api/setup/start", json!({"client_name":client_name}))
                .await?,
        )
        .map_err(|_| "Server returned an incomplete setup request.")?;
        Ok::<_, String>((home, target, api, start))
    };
    let (home, target, api, start) = tokio::select! {
        result = prepared => result?,
        _ = interrupt.recv() => return Ok(false),
        _ = terminate.recv() => return Ok(false),
    };
    let mut console = Terminal;
    console.say(&format!(
        "正在打开 {} 的 GitHub 登录授权。",
        target.portal.origin().ascii_serialization()
    ));
    let flow = async {
        if start.device_code.len() != 43
            || !start
                .device_code
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
            || start.user_code.is_empty()
            || start.user_code.len() > 32
            || !start
                .user_code
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'-')
            || start.expires_in == 0
            || start.expires_in > SETUP_TTL.as_secs()
        {
            return Err("Server returned invalid setup authorization fields.".into());
        }
        let verification = verification_url(
            &api.base,
            &start.verification_uri_complete,
            &start.user_code,
        )?;
        console.say(&format!(
            "请核对浏览器中的配对码：{}\n{}",
            start.user_code, verification
        ));
        if let Err(message) = console.open(verification.as_str()).await {
            console.say(&message);
        }
        let account = wait_authorized(&api, &start, &mut console).await?;
        complete(options, &home, &target, &api, &start, account, &mut console).await?;
        Ok::<bool, String>(true)
    };
    // Once the secret is known, every completion and cancellation attempts revocation.
    let outcome = tokio::select! {
        result = flow => result,
        _ = interrupt.recv() => Ok(false),
        _ = terminate.recv() => Ok(false),
    };
    let _ = tokio::time::timeout(Duration::from_secs(3), api.cancel(&start.device_code)).await;
    outcome
}

/// Probe only the SSH registration handshake; it never starts local harnesses.
pub(super) async fn probe(options: &Options) -> Result<bool, String> {
    probe_mode(options, Duration::from_secs(30), options.daemon).await
}

async fn probe_mode(options: &Options, limit: Duration, batch: bool) -> Result<bool, String> {
    use crate::wire::{Frame, Mode, VERSION, read_frame, write_frame};
    use tokio::io::BufReader;
    let metadata = options.metadata().map_err(|e| e.to_string())?;
    let mut interrupt = crate::platform::interrupt_signal().map_err(|e| e.to_string())?;
    let mut terminate = crate::platform::terminate_signal().map_err(|e| e.to_string())?;
    let mut command = options.command();
    if batch {
        let program = command.as_std().get_program().to_os_string();
        let args = command
            .as_std()
            .get_args()
            .map(OsString::from)
            .collect::<Vec<_>>();
        command = Command::new(program);
        command
            .args(["-o", "BatchMode=yes"])
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true);
    }
    // Own the complete SSH process tree. Interactive Unix prompts use a private
    // ASKPASS bridge to the parent's verified foreground terminal.
    command.stderr(Stdio::inherit());
    let mut process = probe_process::ProbeProcess::spawn(&mut command, !batch)
        .map_err(|e| format!("Could not start OpenSSH: {e}"))?;
    let child = process.child_mut();
    let mut writer = child.stdin.take().ok_or("SSH stdin unavailable")?;
    let mut reader = BufReader::new(child.stdout.take().ok_or("SSH stdout unavailable")?);
    let negotiation = async {
        write_frame(
            &mut writer,
            &Frame::Hello {
                version: VERSION,
                mode: Mode::Shell,
                metadata: metadata.clone(),
            },
        )
        .await
        .map_err(|e| e.to_string())?;
        match read_frame(&mut reader).await.map_err(|e| e.to_string())? {
            Some(Frame::Ready {
                login_code,
                login_expires_in,
                ..
            }) => {
                if !metadata.allow.is_empty() && (!login_code.is_empty() || login_expires_in != 0) {
                    return Err(
                        "Server did not acknowledge GitHub access policy; update ssmcpd.".into(),
                    );
                }
                Ok(true)
            }
            Some(Frame::Reject { message }) => Err(safe_line(&message)),
            _ => Err("SSH did not complete registration.".into()),
        }
    };
    let result = tokio::select! {
        result = tokio::time::timeout(limit, negotiation) => result.map_err(|_| "SSH registration reached its deadline.".to_owned()).and_then(|v| v),
        _ = interrupt.recv() => Ok(false),
        _ = terminate.recv() => Ok(false),
    };
    // Every deadline/cancel reaches explicit process-tree cleanup. Dropping
    // ProbeProcess also terminates its owner and restores the terminal.
    drop(writer);
    drop(reader);
    process.shutdown().await;
    result
}

pub(super) async fn verify_after_setup(options: &Options) -> Result<bool, String> {
    let deadline = Instant::now() + Duration::from_secs(45);
    let mut tries = 0;
    loop {
        tries += 1;
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("SSH verification reached its 45-second limit; the saved configuration is ready for a later retry.".into());
        }
        match probe_mode(options, remaining.min(Duration::from_secs(30)), false).await {
            Ok(false) => return Ok(false),
            Ok(true) => {
                if options.daemon {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return Err("Unattended SSH verification reached its deadline.".into());
                    }
                    return probe_mode(options, remaining.min(Duration::from_secs(15)), true).await
                        .map_err(|error| format!("Interactive SSH works, but unattended authentication failed: {error}. Use a dedicated setup key or load the selected encrypted key into an agent available to the background service."));
                }
                return Ok(true);
            }
            Err(error) if Instant::now() < deadline && tries < 6 => {
                render::status(
                    "setup",
                    "Progress",
                    &format!(
                        "Waiting for SSH key provisioning ({tries}/6): {}",
                        safe_line(&error)
                    ),
                );
                tokio::time::sleep(
                    Duration::from_secs(2).min(deadline.saturating_duration_since(Instant::now())),
                )
                .await;
            }
            Err(error) => {
                return Err(format!(
                    "SSH config is saved, but connection verification did not complete: {error}. Retry ssmcp with the printed alias; setup will not replace your key."
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portal_keeps_dns_identity_when_ssh_routes_to_an_ip() {
        assert_eq!(
            inferred_portal_host("relay.example.com", "192.0.2.10"),
            "relay.example.com"
        );
        assert_eq!(
            inferred_portal_host("relay", "gateway.example.com"),
            "gateway.example.com"
        );
        assert_eq!(inferred_portal_host("127.0.0.1", "127.0.0.1"), "127.0.0.1");
        assert_eq!(inferred_portal_host("[::1]", "::1"), "::1");
    }

    #[test]
    fn portal_labels_respect_utf8_byte_limits_without_splitting_characters() {
        let label = protocol_label(&"开发机".repeat(80));
        assert!(label.len() <= 80);
        assert!(!label.is_empty());
        assert!(
            !protocol_label("line\nwith\rcontrols")
                .chars()
                .any(char::is_control)
        );
        assert_eq!(protocol_label(""), "computer-use");
    }

    fn key() -> String {
        let mut bytes = b"\0\0\0\x0bssh-ed25519\0\0\0\x20".to_vec();
        bytes.extend([7u8; 32]);
        format!("ssh-ed25519 {} fixture", STANDARD.encode(bytes))
    }
    #[test]
    fn authorization_cannot_redirect_to_another_origin_or_put_secret_in_url() {
        let base = portal_url("https://ssmcp.example").unwrap();
        assert!(
            verification_url(
                &base,
                "https://ssmcp.example/setup?code=AAAAA-BBBBB",
                "AAAAA-BBBBB"
            )
            .is_ok()
        );
        for url in [
            "https://evil.example/setup?code=AAAAA-BBBBB",
            "https://ssmcp.example/setup?code=AAAAA-BBBBB&token=secret",
            "http://ssmcp.example/setup?code=AAAAA-BBBBB",
            "https://ssmcp.example/account/keys?code=AAAAA-BBBBB",
        ] {
            assert!(verification_url(&base, url, "AAAAA-BBBBB").is_err());
        }
        for url in [
            "http://example.com",
            "https://user:secret@example.com",
            "https://example.com/mcp",
            "https://example.com?x=1",
        ] {
            assert!(portal_url(url).is_err());
        }
        assert!(portal_url("http://127.0.0.1:12345").is_ok());
        assert!(portal_url("http://[::1]:12345").is_ok());
        assert!(portal_url("http://localhost").is_err());
    }
    #[test]
    fn public_key_validation_preserves_canonical_bytes_and_rejects_private_or_multiple_keys() {
        let (canonical, fingerprint) = parse_public_key(&key()).unwrap();
        assert!(!canonical.ends_with("fixture"));
        assert!(fingerprint.starts_with("SHA256:"));
        for invalid in [
            format!("{}\n{}", key(), key()),
            "-----BEGIN OPENSSH PRIVATE KEY-----".into(),
            format!("command=\"evil\" {}", key()),
            "ssh-ed25519 AAAA".into(),
        ] {
            assert!(parse_public_key(&invalid).is_err());
        }
    }
    #[test]
    fn only_affirmative_input_authorizes_key_creation_and_installation() {
        for accepted in ["yes", "YES", " y "] {
            assert!(yes(accepted));
        }
        for declined in ["", "no", "maybe", "yes; run"] {
            assert!(!yes(declined));
        }
    }
    #[test]
    fn returned_account_cannot_change_identity_or_inject_ssh_configuration() {
        let (public_key, fingerprint) = parse_public_key(&key()).unwrap();
        let mut account = Account {
            github_login: "octocat".into(),
            username: "gh-42".into(),
            keys: vec![AccountKey {
                public_key,
                fingerprint,
            }],
        };
        account.validate().unwrap();
        account.username = "user\nProxyCommand evil".into();
        assert!(account.validate().is_err());
        account.username = "gh-42".into();
        account.keys[0].fingerprint.push('x');
        assert!(account.validate().is_err());
    }
    #[tokio::test]
    async fn api_refuses_redirects_and_oversized_bodies_without_forwarding_device_secret() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for response in [
            "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/steal\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_owned(),
            format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", API_LIMIT + 1),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let task = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut input = [0u8; 4096];
                let _ = socket.read(&mut input).await.unwrap();
                socket.write_all(response.as_bytes()).await.unwrap();
            });
            let api = Api::new(portal_url(&format!("http://127.0.0.1:{port}")).unwrap()).unwrap();
            assert!(api.post("api/setup/poll", json!({"device_code":"secret"})).await.is_err());
            task.await.unwrap();
        }
    }
}
