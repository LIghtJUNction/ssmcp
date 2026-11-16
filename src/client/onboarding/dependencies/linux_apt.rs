//! APT resolves dependencies; this module fetches their exact bytes through
//! bounded public routes. The final normal APT transaction retains CheckAuth.
//! --print-uris is a plan, not proof of authenticity; --no-download must not be
//! used for application because some APT versions clear its authentication items.

use super::TemporaryDirectory;
use crate::backend::process::Managed;
use reqwest::{Client, Url};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::{Read, Write},
    net::IpAddr,
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::Path,
    process::Stdio,
    time::Duration,
};
use tokio::{io::AsyncReadExt, process::Command, time::Instant};

const MAX_FILE: u64 = 64 * 1024 * 1024;
const MAX_TOTAL: u64 = 256 * 1024 * 1024;
const MAX_PACKAGES: usize = 128;
const PLAN_LIMIT: usize = 256 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Archive {
    uri: String,
    name: String,
    size: u64,
    sha256: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Distribution {
    Debian,
    Ubuntu,
    Other,
}

#[derive(Clone, Copy)]
struct Limits {
    connect: Duration,
    first: Duration,
    idle: Duration,
    window: Duration,
    total: Duration,
    min_rate: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(8),
            first: Duration::from_secs(12),
            idle: Duration::from_secs(12),
            window: Duration::from_secs(12),
            total: Duration::from_secs(120),
            min_rate: 65_536,
        }
    }
}

pub(super) async fn install(apt: &Path) -> Result<(), String> {
    let directory = TemporaryDirectory::create(&std::env::temp_dir())?;
    let root = directory.0.canonicalize().map_err(|e| e.to_string())?;
    let plan_cache = root.join("plan");
    let archives = root.join("archives");
    let methods = root.join("no-acquisition-methods");
    for path in [&plan_cache, &archives, &methods] {
        crate::platform::private_directory(path).map_err(|e| e.to_string())?;
    }
    let plan = query_plan(apt, &plan_cache).await?;
    let distribution = distribution();
    let deadline = Instant::now() + Duration::from_secs(600);
    for archive in &plan {
        eprintln!(
            "[ssmcp setup] Fetching verified package bytes: {}",
            archive.name
        );
        let urls = routes(archive, distribution)?;
        let body = fetch_routes(archive, &urls, Limits::default(), deadline).await?;
        let path = archives.join(&archive.name);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path)
            .map_err(|e| e.to_string())?;
        file.write_all(&body)
            .and_then(|_| file.sync_all())
            .map_err(|e| e.to_string())?;
    }
    // A separate, empty plan cache makes this comparison independent of which
    // archives have already been fetched. Changed dependencies fail closed.
    let current = query_plan(apt, &plan_cache).await?;
    same_plan(&plan, &current)?;
    verify_cache(&archives, &plan)?;
    let configuration = root.join("offline-apply.conf");
    fs::write(&configuration, offline_configuration(&archives, &methods)?)
        .map_err(|e| e.to_string())?;
    fs::set_permissions(&configuration, fs::Permissions::from_mode(0o600))
        .map_err(|e| e.to_string())?;
    eprintln!(
        "[ssmcp setup] Asking APT to authenticate and install the cached OpenSSH packages; network acquisition is disabled."
    );
    // Keep the normal acquisition/authentication path, but provide no transport
    // executables. Cached packages still undergo APT's native authentication and
    // hash checks. Missing packages cannot trigger an HTTP/HTTPS download.
    // Never put a deadline around the package-application transaction.
    super::elevated(
        apt,
        &[
            "-c",
            configuration
                .to_str()
                .ok_or("APT configuration path is not UTF-8")?,
            "--assume-yes",
            "--no-allow-unauthenticated",
            "--no-remove",
            "--reinstall",
            "install",
            "openssh-client",
        ],
        false,
    )
    .await
}

fn distribution() -> Distribution {
    let Ok(contents) = fs::read("/etc/os-release") else {
        return Distribution::Other;
    };
    if contents.len() > 64 * 1024 {
        return Distribution::Other;
    }
    let contents = String::from_utf8_lossy(&contents);
    match contents.lines().find_map(|line| {
        line.strip_prefix("ID=")
            .map(|value| value.trim().trim_matches('"'))
    }) {
        Some("debian") => Distribution::Debian,
        Some("ubuntu") => Distribution::Ubuntu,
        _ => Distribution::Other,
    }
}

async fn bounded_read<R: tokio::io::AsyncRead + Unpin>(reader: R) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    reader
        .take((PLAN_LIMIT + 1) as u64)
        .read_to_end(&mut bytes)
        .await
        .map_err(|e| e.to_string())?;
    if bytes.len() > PLAN_LIMIT {
        return Err("APT planning output exceeded 256 KiB".into());
    }
    Ok(bytes)
}

async fn query_plan(apt: &Path, cache: &Path) -> Result<Vec<Archive>, String> {
    let mut command = Command::new(apt);
    command
        .args([
            "--print-uris",
            "--download-only",
            "--assume-yes",
            "--no-allow-unauthenticated",
            "--no-remove",
            "--reinstall",
            "-o",
            "Acquire::ForceHash=sha256",
            "-o",
            "APT::Color=false",
            "-o",
            "Debug::NoLocking=true",
            "-o",
        ])
        .arg(format!("Dir::Cache::archives={}", cache.display()))
        .args(["install", "openssh-client"])
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut process = Managed::spawn(&mut command).map_err(|e| e.to_string())?;
    let stdout = process
        .child
        .stdout
        .take()
        .ok_or("APT stdout is unavailable")?;
    let stderr = process
        .child
        .stderr
        .take()
        .ok_or("APT stderr is unavailable")?;
    let result = tokio::time::timeout(Duration::from_secs(20), async {
        let (stdout, _stderr, status) = tokio::try_join!(
            bounded_read(stdout),
            bounded_read(stderr),
            async { process.wait().await.map_err(|e| e.to_string()) },
        )?;
        if !status.success() {
            // Do not echo arbitrary repository/proxy credentials from stderr.
            return Err(format!(
                "APT could not resolve the OpenSSH package plan ({status}); refresh this distribution's authenticated package indexes and retry"
            ));
        }
        let text = std::str::from_utf8(&stdout).map_err(|_| "APT plan is not UTF-8")?;
        parse_plan(text)
    })
    .await;
    process.shutdown().await;
    result.map_err(|_| "APT planning reached its 20-second deadline".to_owned())?
}

fn parse_plan(text: &str) -> Result<Vec<Archive>, String> {
    if text.len() > PLAN_LIMIT {
        return Err("APT plan is too large".into());
    }
    let mut files = BTreeMap::new();
    let mut total = 0_u64;
    for line in text.lines() {
        let line = line.trim();
        if !line.starts_with('\'') {
            continue;
        }
        let (uri, rest) = line[1..]
            .split_once("' ")
            .ok_or("APT emitted an invalid package URI record")?;
        let fields: Vec<_> = rest.split_ascii_whitespace().collect();
        if fields.len() != 3 {
            return Err("APT package record has unexpected fields".into());
        }
        public_uri(uri)?;
        let name = fields[0];
        if name.is_empty()
            || name.len() > 240
            || name.starts_with('.')
            || !name.ends_with(".deb")
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._%+:-".contains(&byte))
        {
            return Err("APT emitted an unsafe cache filename".into());
        }
        let size = fields[1]
            .parse::<u64>()
            .map_err(|_| "Invalid APT package size")?;
        if size == 0 || size > MAX_FILE {
            return Err("APT package exceeds the 64 MiB size limit".into());
        }
        let hash = fields[2]
            .strip_prefix("SHA256:")
            .ok_or("APT did not supply a SHA256 digest; weaker hashes are not accepted")?;
        if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("APT supplied an invalid SHA256 digest".into());
        }
        let archive = Archive {
            uri: uri.to_owned(),
            name: name.to_owned(),
            size,
            sha256: hash.to_ascii_lowercase(),
        };
        if let Some(previous) = files.get(name) {
            if previous != &archive {
                return Err("APT emitted conflicting records for one cache filename".into());
            }
            continue;
        }
        total = total.checked_add(size).ok_or("APT package size overflow")?;
        if total > MAX_TOTAL || files.len() >= MAX_PACKAGES {
            return Err("APT dependency plan exceeds 128 packages or 256 MiB".into());
        }
        files.insert(name.to_owned(), archive);
    }
    // --reinstall into a fresh empty plan cache must name the OpenSSH archive.
    // An empty/misconfigured print-only result must never enable a naked install.
    if !files.keys().any(|name| name.starts_with("openssh-client_")) {
        return Err("APT plan contains no openssh-client archive".into());
    }
    Ok(files.into_values().collect())
}

fn public_uri(value: &str) -> Result<Url, String> {
    let url = Url::parse(value).map_err(|_| "Invalid package URL")?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || value.chars().any(char::is_control)
    {
        return Err(
            "Package URLs must be public HTTP(S) URLs without credentials or query data".into(),
        );
    }
    let host = url.host_str().ok_or("Package URL has no host")?;
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty()
        || !host.contains('.')
        || host == "localhost"
        || [".localhost", ".local", ".internal", ".test", ".invalid"]
            .iter()
            .any(|suffix| host.ends_with(suffix))
        || host.parse::<IpAddr>().is_ok()
    {
        return Err("Package URLs must name a public repository host, not a local address".into());
    }
    if url.port().is_some_and(|port| {
        (url.scheme() == "http" && port != 80) || (url.scheme() == "https" && port != 443)
    }) {
        return Err("Package URLs must use the standard HTTP(S) repository port".into());
    }
    Ok(url)
}

fn routes(archive: &Archive, distribution: Distribution) -> Result<Vec<String>, String> {
    let mut original = public_uri(&archive.uri)?;
    // Debian/Ubuntu's common HTTP archive endpoints also support HTTPS. An
    // unsupported endpoint fails closed instead of sending data to a new host.
    original
        .set_scheme("https")
        .map_err(|_| "Invalid package URL scheme")?;
    original
        .set_port(None)
        .map_err(|_| "Invalid package URL port")?;
    let mut result = vec![original.to_string()];
    let host = original.host_str().ok_or("Package URL has no host")?;
    let university = matches!(host, "mirrors.tuna.tsinghua.edu.cn" | "mirrors.ustc.edu.cn");
    let roots: &[(&str, &str)] = match distribution {
        Distribution::Debian
            if host.ends_with(".debian.org") || host == "deb.debian.org" || university =>
        {
            &[
                ("/debian-security/", "debian-security"),
                ("/debian/", "debian"),
            ]
        }
        Distribution::Ubuntu if host.ends_with(".ubuntu.com") || university => {
            &[("/ubuntu-ports/", "ubuntu-ports"), ("/ubuntu/", "ubuntu")]
        }
        _ => &[],
    };
    if let Some((prefix, repository)) = roots
        .iter()
        .find(|(prefix, _)| original.path().starts_with(prefix))
    {
        let suffix = &original.path()[prefix.len()..];
        if !suffix.starts_with("pool/") {
            return Ok(result);
        }
        for base in [
            format!("https://mirrors.tuna.tsinghua.edu.cn/{repository}/"),
            format!("https://mirrors.ustc.edu.cn/{repository}/"),
        ] {
            let candidate = format!("{base}{suffix}");
            public_uri(&candidate)?;
            if !result.contains(&candidate) {
                result.push(candidate);
            }
        }
    }
    Ok(result)
}

fn same_plan(expected: &[Archive], actual: &[Archive]) -> Result<(), String> {
    if expected != actual {
        return Err("APT dependencies or repository URLs changed during download; no packages were installed. Retry setup.".into());
    }
    Ok(())
}

fn quoted_path(path: &Path) -> Result<String, String> {
    let value = path.to_str().ok_or("APT temporary path is not UTF-8")?;
    if !path.is_absolute() || value.chars().any(char::is_control) || value.contains(['"', '\\']) {
        return Err("APT temporary paths cannot contain configuration syntax".into());
    }
    Ok(format!("\"{value}\""))
}

fn offline_configuration(archives: &Path, methods: &Path) -> Result<String, String> {
    Ok(format!(
        "#clear Dir::Bin::Methods;\nDir::Bin::Methods {};\nDir::Cache::archives {};\nAPT::Get::Download \"true\";\nAPT::Get::Download-Only \"false\";\nAPT::Get::Print-URIs \"false\";\nAPT::Get::Simulate \"false\";\nAPT::Get::Assume-Yes \"true\";\nAPT::Get::AllowUnauthenticated \"false\";\nAPT::Get::Force-Yes \"false\";\nAPT::Get::allow-Downgrades \"false\";\nAPT::Get::allow-Remove-Essential \"false\";\nAPT::Get::allow-Change-Held-Packages \"false\";\nDebug::NoLocking \"false\";\n",
        quoted_path(methods)?,
        quoted_path(archives)?
    ))
}

fn verify_cache(directory: &Path, plan: &[Archive]) -> Result<(), String> {
    for archive in plan {
        let path = directory.join(&archive.name);
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)
            .map_err(|e| e.to_string())?;
        let metadata = file.metadata().map_err(|e| e.to_string())?;
        if !metadata.is_file() || metadata.len() != archive.size || metadata.nlink() != 1 {
            return Err("APT cache file changed before installation".into());
        }
        let mut digest = Sha256::new();
        let mut buffer = [0_u8; 65_536];
        loop {
            let count = file.read(&mut buffer).map_err(|e| e.to_string())?;
            if count == 0 {
                break;
            }
            digest.update(&buffer[..count]);
        }
        if format!("{:x}", digest.finalize()) != archive.sha256 {
            return Err("APT cache digest changed before installation".into());
        }
    }
    Ok(())
}

async fn fetch_routes(
    archive: &Archive,
    routes: &[String],
    limits: Limits,
    deadline: Instant,
) -> Result<Vec<u8>, String> {
    let mut last = String::new();
    for (index, route) in routes.iter().enumerate() {
        if Instant::now() >= deadline {
            return Err("APT package downloads reached the ten-minute shared deadline".into());
        }
        match fetch(route, archive, limits, deadline).await {
            Ok(bytes) => return Ok(bytes),
            Err(error) => {
                last = error;
                if index + 1 < routes.len() {
                    eprintln!(
                        "[ssmcp setup] Package source failed, stalled, stayed slow, or failed verification; trying the next public mirror."
                    );
                }
            }
        }
    }
    Err(format!(
        "All public routes for {} failed: {last}",
        archive.name
    ))
}

async fn fetch(
    initial: &str,
    archive: &Archive,
    limits: Limits,
    shared_deadline: Instant,
) -> Result<Vec<u8>, String> {
    let deadline = shared_deadline.min(Instant::now() + limits.total);
    let client = Client::builder()
        .connect_timeout(limits.connect)
        .redirect(reqwest::redirect::Policy::none());
    // Loopback fixtures are isolated from the runner's proxy configuration.
    // Production retains reqwest's HTTP(S)_PROXY and NO_PROXY support.
    #[cfg(test)]
    let client = client.no_proxy();
    let client = client.build().map_err(|e| e.to_string())?;
    let mut url = Url::parse(initial).map_err(|_| "Invalid package URL")?;
    let mut redirects = 0;
    let mut response = loop {
        let first = (Instant::now() + limits.first).min(deadline);
        let response = tokio::time::timeout_at(first, client.get(url.clone()).send())
            .await
            .map_err(|_| "Package source reached the response-header deadline")?
            .map_err(|_| "Package source connection failed")?;
        if response.status().is_redirection() {
            redirects += 1;
            if redirects > 4 {
                return Err("Package redirect limit exceeded".into());
            }
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .ok_or("Package redirect has no valid location")?;
            let next = url
                .join(location)
                .map_err(|_| "Invalid package redirect URL")?;
            public_uri(next.as_str())?;
            if next.scheme() != "https" {
                return Err("Package redirect attempted an HTTPS downgrade".into());
            }
            url = next;
            continue;
        }
        break response
            .error_for_status()
            .map_err(|_| "Package source returned an HTTP error")?;
    };
    if response
        .content_length()
        .is_some_and(|size| size != archive.size)
    {
        return Err("Package content length differs from the APT plan".into());
    }
    let mut bytes = Vec::with_capacity(archive.size as usize);
    let mut window = Instant::now();
    let mut window_bytes = 0_u64;
    loop {
        let due = (Instant::now() + limits.idle).min(deadline);
        let chunk = tokio::time::timeout_at(due, response.chunk())
            .await
            .map_err(|_| "Package transfer stalled or reached its deadline")?
            .map_err(|_| "Package transfer failed")?;
        let Some(chunk) = chunk else {
            break;
        };
        if (bytes.len() as u64).saturating_add(chunk.len() as u64) > archive.size {
            return Err("Package transfer exceeded the declared size".into());
        }
        window_bytes += chunk.len() as u64;
        bytes.extend_from_slice(&chunk);
        let elapsed = window.elapsed();
        if elapsed >= limits.window {
            if (window_bytes as f64) / elapsed.as_secs_f64() < limits.min_rate as f64 {
                return Err("Package transfer stayed below the minimum rate".into());
            }
            window = Instant::now();
            window_bytes = 0;
        }
        if Instant::now() >= deadline {
            return Err("Package transfer reached its deadline".into());
        }
    }
    if bytes.len() as u64 != archive.size
        || format!("{:x}", Sha256::digest(&bytes)) != archive.sha256
    {
        return Err("Package size or SHA256 does not match the APT plan".into());
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    fn archive(bytes: &[u8]) -> Archive {
        Archive {
            uri: "https://deb.debian.org/debian/pool/main/o/openssh/openssh-client_2.3_amd64.deb"
                .into(),
            name: "openssh-client_1%3a2.3_amd64.deb".into(),
            size: bytes.len() as u64,
            sha256: format!("{:x}", Sha256::digest(bytes)),
        }
    }

    fn line(archive: &Archive) -> String {
        format!(
            "'{}' {} {} SHA256:{}\n",
            archive.uri, archive.name, archive.size, archive.sha256
        )
    }

    #[test]
    fn real_apt_record_keeps_the_encoded_epoch_cache_name() {
        let expected = archive(b"fixture");
        assert_eq!(
            parse_plan(&format!("Reading package lists...\n{}", line(&expected))).unwrap(),
            vec![expected]
        );
    }

    #[test]
    fn weak_hashes_credentials_method_paths_and_unsafe_filenames_are_rejected() {
        let valid = archive(b"fixture");
        for invalid in [
            line(&valid).replace("SHA256:", "MD5Sum:"),
            line(&valid).replace(
                "https://deb.debian.org",
                "https://user:secret@deb.debian.org",
            ),
            line(&valid).replace("https://deb.debian.org", "../method://deb.debian.org"),
            line(&valid).replace("https://deb.debian.org", "http://127.0.0.1"),
            line(&valid).replace(".deb' ", ".deb?token=secret' "),
            line(&valid).replace(&valid.name, "../openssh-client_bad.deb"),
            line(&valid).replace(&valid.size.to_string(), "0"),
        ] {
            assert!(parse_plan(&invalid).is_err(), "{invalid}");
        }
        assert!(parse_plan("Reading package lists...\n").is_err());
    }

    #[test]
    fn mirrors_keep_the_package_path_and_do_not_remap_unknown_repositories() {
        let package = archive(b"fixture");
        let known = routes(&package, Distribution::Debian).unwrap();
        assert_eq!(known.len(), 3);
        for route in known {
            assert_eq!(
                Url::parse(&route).unwrap().path(),
                Url::parse(&package.uri).unwrap().path()
            );
        }
        let mut private = package.clone();
        private.uri = private
            .uri
            .replace("deb.debian.org", "packages.example.org");
        assert_eq!(routes(&private, Distribution::Debian).unwrap().len(), 1);
        assert_eq!(routes(&package, Distribution::Other).unwrap().len(), 1);
    }

    #[test]
    fn changed_plan_and_cache_tampering_never_reach_application() {
        let temp = TemporaryDirectory::create(&std::env::temp_dir()).unwrap();
        let package = archive(b"fixture");
        fs::write(temp.0.join(&package.name), b"fixture").unwrap();
        verify_cache(&temp.0, std::slice::from_ref(&package)).unwrap();
        fs::write(temp.0.join(&package.name), b"changed").unwrap();
        assert!(verify_cache(&temp.0, std::slice::from_ref(&package)).is_err());
        let mut changed = package.clone();
        changed.uri = changed.uri.replace("deb.debian.org", "security.debian.org");
        assert!(same_plan(&[package], &[changed]).is_err());
    }

    #[test]
    fn final_configuration_keeps_authentication_and_clears_method_overrides() {
        let config =
            offline_configuration(Path::new("/tmp/cache"), Path::new("/tmp/empty")).unwrap();
        assert!(config.starts_with("#clear Dir::Bin::Methods;\n"));
        assert!(config.contains("APT::Get::Download \"true\";"));
        assert!(config.contains("APT::Get::AllowUnauthenticated \"false\";"));
        assert!(config.contains("APT::Get::Force-Yes \"false\";"));
        assert!(!config.contains("--no-download"));
        assert!(
            offline_configuration(Path::new("/tmp/bad\"path"), Path::new("/tmp/empty")).is_err()
        );
    }

    async fn server(
        bytes: Vec<u8>,
        slow: bool,
        count: Arc<AtomicUsize>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            count.fetch_add(1, Ordering::SeqCst);
            let mut request = [0_u8; 4096];
            let _ = socket.read(&mut request).await;
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                bytes.len()
            );
            if socket.write_all(header.as_bytes()).await.is_err() {
                return;
            }
            if slow {
                for byte in bytes {
                    if socket.write_all(&[byte]).await.is_err() {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            } else {
                let _ = socket.write_all(&bytes).await;
            }
        });
        (format!("http://{address}/package.deb"), task)
    }

    #[tokio::test]
    async fn sustained_low_rate_tries_the_next_route_and_verifies_its_bytes() {
        let bytes = vec![b'x'; 64];
        let slow_count = Arc::new(AtomicUsize::new(0));
        let fast_count = Arc::new(AtomicUsize::new(0));
        let (slow, slow_task) = server(bytes.clone(), true, slow_count.clone()).await;
        let (fast, fast_task) = server(bytes.clone(), false, fast_count.clone()).await;
        let limits = Limits {
            connect: Duration::from_secs(1),
            first: Duration::from_secs(1),
            idle: Duration::from_millis(200),
            window: Duration::from_millis(80),
            total: Duration::from_secs(2),
            min_rate: 1024,
        };
        let fetched = fetch_routes(
            &archive(&bytes),
            &[slow, fast],
            limits,
            Instant::now() + Duration::from_secs(3),
        )
        .await
        .unwrap();
        assert_eq!(fetched, bytes);
        assert_eq!(slow_count.load(Ordering::SeqCst), 1);
        assert_eq!(fast_count.load(Ordering::SeqCst), 1);
        slow_task.abort();
        fast_task.await.unwrap();
    }

    #[tokio::test]
    async fn same_length_tampered_mirror_bytes_are_rejected() {
        let expected = archive(b"expected");
        let (url, task) = server(b"tampered".to_vec(), false, Arc::new(AtomicUsize::new(0))).await;
        let result = fetch_routes(
            &expected,
            &[url],
            Limits::default(),
            Instant::now() + Duration::from_secs(3),
        )
        .await;
        assert!(result.unwrap_err().contains("SHA256"));
        task.await.unwrap();
    }
}
