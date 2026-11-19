//! Cached update notices and explicit, checksum-verified self-update.
use std::{
    cmp::Ordering,
    ffi::OsString,
    path::{Path, PathBuf},
    time::Duration,
};

const API: &str = "https://api.github.com/repos/LIghtJUNction/ssmcp/releases/latest";
const MAX_METADATA: usize = 1024 * 1024;
const HELP: &str = "\
Usage: ssmcp update [--check]

Update this installed client to the latest stable GitHub Release.
  --check     Check for updates without downloading or changing files
  -h, --help  Show this help

Downloads use the bundled installer, official SHA256SUMS and a version check.
Older releases never replace a newer client. Package-manager installations
must be updated with their original package manager.
Existing sessions keep their current executable until restarted.
";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Mode {
    Install,
    Check,
    Help,
}

fn mode(args: &[OsString]) -> crate::Result<Mode> {
    match args {
        [] => Ok(Mode::Install),
        [arg] if arg == "--check" => Ok(Mode::Check),
        [arg] if arg == "--help" || arg == "-h" => Ok(Mode::Help),
        _ => Err("Usage: ssmcp update [--check]; use --help for details".into()),
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct Version([u64; 3]);

impl Version {
    fn parse(value: &str) -> crate::Result<Self> {
        let mut parts = value.strip_prefix('v').unwrap_or(value).split('.');
        let mut numbers = [0; 3];
        for number in &mut numbers {
            let part = parts
                .next()
                .ok_or("release version must be MAJOR.MINOR.PATCH")?;
            if part.is_empty()
                || !part.bytes().all(|b| b.is_ascii_digit())
                || (part.len() > 1 && part.starts_with('0'))
            {
                return Err("release version must be stable MAJOR.MINOR.PATCH".into());
            }
            *number = part
                .parse()
                .map_err(|_| "release version number is too large")?;
        }
        if parts.next().is_some() {
            return Err("release version must be MAJOR.MINOR.PATCH".into());
        }
        Ok(Self(numbers))
    }
    fn tag(self) -> String {
        format!("v{}.{}.{}", self.0[0], self.0[1], self.0[2])
    }
}

#[derive(serde::Deserialize)]
struct Release {
    tag_name: String,
    draft: bool,
    prerelease: bool,
}

fn release_version(bytes: &[u8]) -> crate::Result<Version> {
    let release: Release = serde_json::from_slice(bytes)?;
    if release.draft || release.prerelease {
        return Err("GitHub did not return a published stable release".into());
    }
    Version::parse(&release.tag_name)
}

async fn latest_release(client: &reqwest::Client, url: &str) -> crate::Result<Version> {
    let mut response = client
        .get(url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await?
        .error_for_status()?;
    if response
        .content_length()
        .is_some_and(|size| size > MAX_METADATA as u64)
    {
        return Err("GitHub release metadata exceeds 1 MiB".into());
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if chunk.len() > MAX_METADATA.saturating_sub(body.len()) {
            return Err("GitHub release metadata exceeds 1 MiB".into());
        }
        body.extend_from_slice(&chunk);
    }
    release_version(&body)
}

const NOTICE_TTL: u64 = 24 * 60 * 60;
const NOTICE_BACKOFF: u64 = 60 * 60;
const NOTICE_CACHE_LIMIT: u64 = 4096;
const NOTICE_CACHE_FILE: &str = "release.json";
static NOTICE_SHOWN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct NoticeCache {
    schema: u8,
    checked_at: u64,
    succeeded: bool,
    latest: Option<String>,
}

struct NoticeCheck {
    directory: super::interface::history::SafeDirectory,
    _lock: super::interface::history::HistoryLock,
    previous: Option<Version>,
}

enum NoticePreparation {
    Cached(Option<Version>),
    Check(NoticeCheck),
}

fn notice_cache_directory() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .map(|path| path.join("ssmcp/cache/update"))
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .or_else(|| {
                std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .filter(|path| path.is_absolute())
                    .map(|path| path.join(".cache"))
            })
            .map(|path| path.join("ssmcp/update"))
    }
}

fn read_notice_cache(
    directory: &super::interface::history::SafeDirectory,
    now: u64,
) -> Option<NoticeCache> {
    use std::io::Read;
    let mut bytes = Vec::new();
    directory
        .open(NOTICE_CACHE_FILE, false, false, false)
        .ok()?
        .take(NOTICE_CACHE_LIMIT + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > NOTICE_CACHE_LIMIT {
        return None;
    }
    let cache: NoticeCache = serde_json::from_slice(&bytes).ok()?;
    if cache.schema != 1
        || cache.checked_at > now
        || (cache.succeeded && cache.latest.is_none())
        || cache
            .latest
            .as_ref()
            .is_some_and(|tag| Version::parse(tag).is_err())
    {
        return None;
    }
    Some(cache)
}

fn write_notice_cache(
    directory: &super::interface::history::SafeDirectory,
    now: u64,
    succeeded: bool,
    latest: Option<Version>,
) -> Option<()> {
    let bytes = serde_json::to_vec(&NoticeCache {
        schema: 1,
        checked_at: now,
        succeeded,
        latest: latest.map(Version::tag),
    })
    .ok()?;
    directory.write_atomic(NOTICE_CACHE_FILE, &bytes).ok()
}

fn prepare_notice(directory: PathBuf, now: u64) -> Option<NoticePreparation> {
    let directory = super::interface::history::SafeDirectory::at(directory).ok()?;
    // This lock is nonblocking. Another process checking updates is a reason to
    // skip this optional notice, never to delay an interactive session.
    let lock = directory.lock().ok()?;
    let cache = read_notice_cache(&directory, now);
    let previous = cache
        .as_ref()
        .and_then(|cache| cache.latest.as_deref())
        .and_then(|tag| Version::parse(tag).ok());
    if cache.as_ref().is_some_and(|cache| {
        now - cache.checked_at
            < if cache.succeeded {
                NOTICE_TTL
            } else {
                NOTICE_BACKOFF
            }
    }) {
        return Some(NoticePreparation::Cached(previous));
    }
    // Persist the attempt before networking. Timeout, cancellation or process
    // exit still leave a short backoff instead of a request on every reconnect.
    write_notice_cache(&directory, now, false, previous)?;
    Some(NoticePreparation::Check(NoticeCheck {
        directory,
        _lock: lock,
        previous,
    }))
}

fn available_text(current: Version, latest: Option<Version>) -> Option<String> {
    latest.filter(|latest| *latest > current).map(|latest| {
        // Keep the command first so a narrow footer does not hide the action.
        format!("ssmcp update · {} available", latest.tag())
    })
}

async fn notice_at(
    client: reqwest::Client,
    url: &str,
    directory: PathBuf,
    current: Version,
    now: u64,
) -> Option<String> {
    let prepared = tokio::task::spawn_blocking(move || prepare_notice(directory, now))
        .await
        .ok()??;
    let check = match prepared {
        NoticePreparation::Cached(latest) => return available_text(current, latest),
        NoticePreparation::Check(check) => check,
    };
    // This request belongs to the caller's future: aborting the Session probe
    // drops it and its client. No detached request or installer is spawned.
    let result = latest_release(&client, url).await;
    let succeeded = result.is_ok();
    let latest = result.ok().or(check.previous);
    let notice = available_text(current, latest);
    let _ = tokio::task::spawn_blocking(move || {
        let owned = check;
        write_notice_cache(&owned.directory, now, succeeded, latest)
    })
    .await;
    notice
}

pub(super) async fn available_notice() -> Option<String> {
    use std::sync::atomic::Ordering;
    if NOTICE_SHOWN.load(Ordering::Acquire) {
        return None;
    }
    let current = Version::parse(env!("CARGO_PKG_VERSION")).ok()?;
    let directory = notice_cache_directory()?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    let client = reqwest::Client::builder()
        .user_agent(concat!("ssmcp/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(1))
        .timeout(Duration::from_secs(3))
        .pool_max_idle_per_host(0)
        .build()
        .ok()?;
    notice_at(client, API, directory, current, now).await
}

pub(super) fn mark_notice_shown() -> bool {
    !NOTICE_SHOWN.swap(true, std::sync::atomic::Ordering::AcqRel)
}

fn package_manager_hint(executable: &Path, cargo_home: Option<&Path>) -> Option<&'static str> {
    let path = executable
        .to_string_lossy()
        .replace('\\', "/")
        .to_ascii_lowercase();
    if path.contains("/.cargo/bin/")
        || cargo_home.is_some_and(|root| executable.starts_with(root.join("bin")))
        || executable
            .parent()
            .and_then(Path::parent)
            .is_some_and(|root| root.join(".crates2.json").is_file())
    {
        Some(
            "Cargo manages this installation. Run: cargo install ssmcp --locked --bin ssmcp --force",
        )
    } else if path.starts_with("/nix/store/") || path.starts_with("/snap/") {
        Some(
            "A package manager manages this installation. Update its ssmcp package or environment.",
        )
    } else if path.contains("/cellar/ssmcp/")
        || path.contains("/caskroom/ssmcp/")
        || path.starts_with("/opt/homebrew/")
    {
        Some("Homebrew manages this installation. Run: brew upgrade ssmcp")
    } else if path.contains("/scoop/apps/ssmcp/") {
        Some("Scoop manages this installation. Run: scoop update ssmcp")
    } else if path.contains("/chocolatey/")
        || path.contains("/winget/packages/")
        || path.contains("/windowsapps/")
    {
        Some(
            "A Windows package manager manages this installation. Update ssmcp with that package manager.",
        )
    } else if ["/usr/bin/", "/bin/", "/usr/sbin/", "/sbin/"]
        .iter()
        .any(|prefix| path.starts_with(prefix))
    {
        Some(
            "This is a system package installation. Update ssmcp with your system package manager (for example, the AUR helper for ssmcp-git).",
        )
    } else {
        None
    }
}

fn install_directory<'a>(
    executable: &'a Path,
    cargo_home: Option<&Path>,
) -> crate::Result<&'a Path> {
    let expected = if cfg!(windows) { "ssmcp.exe" } else { "ssmcp" };
    if executable.file_name() != Some(std::ffi::OsStr::new(expected)) {
        return Err(format!("Self-update requires an executable named {expected}; use the original installer for this renamed binary.").into());
    }
    if let Some(hint) = package_manager_hint(executable, cargo_home) {
        return Err(hint.into());
    }
    executable
        .parent()
        .ok_or_else(|| "installed executable has no parent directory".into())
}

struct Installer {
    directory: PathBuf,
    script: PathBuf,
}

impl Installer {
    fn prepare(parent: &Path) -> crate::Result<Self> {
        let mut nonce = [0u8; 16];
        getrandom::fill(&mut nonce)
            .map_err(|e| format!("cannot create update staging name: {e}"))?;
        let directory = parent.join(format!(".ssmcp-update-{:032x}", u128::from_le_bytes(nonce)));
        let builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        let builder = {
            use std::os::unix::fs::DirBuilderExt;
            let mut builder = builder;
            builder.mode(0o700);
            builder
        };
        builder.create(&directory).map_err(|e| format!("Cannot write the installation directory {}: {e}. Use the original installer or package manager with the required permissions.", parent.display()))?;
        let script = directory.join(if cfg!(windows) {
            "install.ps1"
        } else {
            "install.sh"
        });
        let installer = Self { directory, script };
        #[cfg(windows)]
        let source = include_str!("../../scripts/install.ps1");
        #[cfg(not(windows))]
        let source = include_str!("../../scripts/install.sh");
        std::fs::write(&installer.script, source)?;
        Ok(installer)
    }
}

impl Drop for Installer {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

fn installer_command(script: &Path, directory: &Path, version: Version) -> tokio::process::Command {
    #[cfg(windows)]
    let mut command = {
        let mut c = tokio::process::Command::new("powershell.exe");
        c.args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ])
        .arg(script)
        .arg("-Version")
        .arg(version.tag())
        .arg("-InstallDir")
        .arg(directory)
        .arg("-NoModifyPath");
        c
    };
    #[cfg(not(windows))]
    let mut command = {
        let mut c = tokio::process::Command::new("/bin/sh");
        c.arg(script)
            .arg("--version")
            .arg(version.tag())
            .arg("--dir")
            .arg(directory);
        c
    };
    command.kill_on_drop(true);
    command
}

async fn run_installer(script: &Path, directory: &Path, version: Version) -> crate::Result<()> {
    let status = installer_command(script, directory, version)
        .status()
        .await?;
    if !status.success() {
        return Err(
            format!("Verified installer failed ({status}); inspect its message above.").into(),
        );
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Outcome {
    Current,
    NewerInstalled,
    Available,
    Installed,
}

async fn update_to(
    current: Version,
    latest: Version,
    check_only: bool,
    executable: &Path,
    cargo_home: Option<&Path>,
) -> crate::Result<Outcome> {
    match latest.cmp(&current) {
        Ordering::Equal => return Ok(Outcome::Current),
        Ordering::Less => return Ok(Outcome::NewerInstalled),
        Ordering::Greater if check_only => return Ok(Outcome::Available),
        Ordering::Greater => {}
    }
    let directory = install_directory(executable, cargo_home)?;
    let installer = Installer::prepare(directory)?;
    println!("Updating ssmcp {} → {}…", current.tag(), latest.tag());
    run_installer(&installer.script, directory, latest).await?;
    Ok(Outcome::Installed)
}

pub(super) async fn run(args: Vec<OsString>) -> crate::Result<()> {
    let mode = mode(&args)?;
    if mode == Mode::Help {
        print!("{HELP}");
        return Ok(());
    }
    let current = Version::parse(env!("CARGO_PKG_VERSION"))?;
    println!("Checking the latest stable ssmcp release…");
    let client = reqwest::Client::builder()
        .user_agent(concat!("ssmcp/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(8))
        .timeout(Duration::from_secs(30))
        .build()?;
    let latest = latest_release(&client, API).await?;
    let executable = std::env::current_exe()?.canonicalize()?;
    let cargo_home = std::env::var_os("CARGO_HOME").map(PathBuf::from);
    match update_to(
        current,
        latest,
        mode == Mode::Check,
        &executable,
        cargo_home.as_deref(),
    )
    .await?
    {
        Outcome::Current => println!(
            "ssmcp {} is already the latest stable release.",
            current.tag()
        ),
        Outcome::NewerInstalled => println!(
            "Installed {} is newer than the latest published release {}. Keeping the installed version.",
            current.tag(),
            latest.tag()
        ),
        Outcome::Available => {
            println!("Update available: {} → {}.", current.tag(), latest.tag());
            println!(
                "{}",
                package_manager_hint(&executable, cargo_home.as_deref())
                    .unwrap_or("Run ssmcp update to install it.")
            );
        }
        Outcome::Installed => println!(
            "Updated ssmcp to {}. Restart existing client sessions when you want them to use the new version.",
            latest.tag()
        ),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }
    fn temporary() -> PathBuf {
        let mut nonce = [0; 16];
        getrandom::fill(&mut nonce).unwrap();
        let path = std::env::temp_dir().join(format!(
            "ssmcp-update-test-{:x}",
            u128::from_le_bytes(nonce)
        ));
        std::fs::create_dir(&path).unwrap();
        path
    }

    struct NoticeServer {
        root: PathBuf,
        url: String,
        requests: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        worker: tokio::task::JoinHandle<()>,
    }
    impl NoticeServer {
        async fn start(status: &str, body: &str) -> Self {
            use std::sync::{
                Arc,
                atomic::{AtomicUsize, Ordering},
            };
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let requests = Arc::new(AtomicUsize::new(0));
            let count = requests.clone();
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let worker = tokio::spawn(async move {
                loop {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let mut request = Vec::new();
                    let mut buffer = [0; 1024];
                    while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                        let size = stream.read(&mut buffer).await.unwrap();
                        if size == 0 {
                            break;
                        }
                        request.extend_from_slice(&buffer[..size]);
                        assert!(request.len() <= 8192);
                    }
                    count.fetch_add(1, Ordering::SeqCst);
                    let _ = stream.write_all(response.as_bytes()).await;
                }
            });
            Self {
                root: temporary(),
                url,
                requests,
                worker,
            }
        }
        fn directory(&self) -> PathBuf {
            self.root.join("cache")
        }
        fn hits(&self) -> usize {
            self.requests.load(std::sync::atomic::Ordering::SeqCst)
        }
        fn client(&self) -> reqwest::Client {
            reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_millis(500))
                .pool_max_idle_per_host(0)
                .build()
                .unwrap()
        }
        async fn check(&self, current: &str, now: u64) -> Option<String> {
            notice_at(self.client(), &self.url, self.directory(), v(current), now).await
        }
    }
    impl Drop for NoticeServer {
        fn drop(&mut self) {
            self.worker.abort();
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
    const NOTICE_RELEASE: &str = r#"{"tag_name":"v1.2.4","draft":false,"prerelease":false}"#;

    #[tokio::test]
    async fn notice_cache_only_announces_newer_versions_and_refreshes_after_a_day() {
        let fixture = NoticeServer::start("200 OK", NOTICE_RELEASE).await;
        let expected = Some("ssmcp update · v1.2.4 available".to_owned());
        assert_eq!(fixture.check("1.2.3", 1000).await, expected);
        assert_eq!(fixture.hits(), 1);
        assert_eq!(fixture.check("1.2.3", 1001).await, expected);
        assert_eq!(fixture.check("1.2.4", 1001).await, None);
        assert_eq!(fixture.check("2.0.0", 1001).await, None);
        assert_eq!(
            fixture.check("1.2.3", 1000 + NOTICE_TTL - 1).await,
            expected
        );
        assert_eq!(fixture.hits(), 1, "a fresh cache never contacts the server");
        assert_eq!(fixture.check("1.2.3", 1000 + NOTICE_TTL).await, expected);
        assert_eq!(fixture.hits(), 2);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let path = fixture.directory().join(NOTICE_CACHE_FILE);
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[tokio::test]
    async fn notice_failures_back_off_without_losing_a_known_newer_release() {
        let failure = NoticeServer::start("503 Unavailable", "offline").await;
        assert_eq!(failure.check("1.2.3", 1000).await, None);
        assert_eq!(
            failure.check("1.2.3", 1000 + NOTICE_BACKOFF - 1).await,
            None
        );
        assert_eq!(failure.hits(), 1);
        assert_eq!(failure.check("1.2.3", 1000 + NOTICE_BACKOFF).await, None);
        assert_eq!(failure.hits(), 2);
        let directory =
            super::super::interface::history::SafeDirectory::at(failure.directory()).unwrap();
        write_notice_cache(&directory, 1000, true, Some(v("1.2.4"))).unwrap();
        let expected = Some("ssmcp update · v1.2.4 available".to_owned());
        assert_eq!(failure.check("1.2.3", 1000 + NOTICE_TTL).await, expected);
        assert_eq!(failure.hits(), 3);
        assert_eq!(failure.check("1.2.3", 1001 + NOTICE_TTL).await, expected);
        assert_eq!(failure.hits(), 3);
    }

    #[tokio::test]
    async fn notice_revalidates_corrupt_oversized_and_future_caches() {
        for content in [
            b"not JSON".to_vec(),
            vec![b'x'; NOTICE_CACHE_LIMIT as usize + 1],
            br#"{"schema":1,"checked_at":1001,"succeeded":true,"latest":"v9.9.9"}"#.to_vec(),
            br#"{"schema":1,"checked_at":1000,"succeeded":true,"latest":"v9.9.9\u001b[31m"}"#
                .to_vec(),
            br#"{"schema":1,"checked_at":1000,"succeeded":true,"latest":null}"#.to_vec(),
        ] {
            let fixture = NoticeServer::start("200 OK", NOTICE_RELEASE).await;
            let directory =
                super::super::interface::history::SafeDirectory::at(fixture.directory()).unwrap();
            directory.write_atomic(NOTICE_CACHE_FILE, &content).unwrap();
            assert_eq!(
                fixture.check("1.2.3", 1000).await,
                Some("ssmcp update · v1.2.4 available".into())
            );
            assert_eq!(fixture.hits(), 1);
        }
    }

    #[tokio::test]
    async fn notice_ignores_untrusted_release_tags_and_busy_cache() {
        for body in [
            r#"{"tag_name":"v9.9.9\u001b[31m","draft":false,"prerelease":false}"#,
            r#"{"tag_name":"v9.9.9","draft":true,"prerelease":false}"#,
            r#"{"tag_name":"v9.9.9","draft":false,"prerelease":true}"#,
        ] {
            let fixture = NoticeServer::start("200 OK", body).await;
            assert_eq!(fixture.check("1.2.3", 1000).await, None);
            assert_eq!(fixture.check("1.2.3", 1001).await, None);
            assert_eq!(fixture.hits(), 1);
        }
        let fixture = NoticeServer::start("200 OK", NOTICE_RELEASE).await;
        let directory =
            super::super::interface::history::SafeDirectory::at(fixture.directory()).unwrap();
        let lock = directory.lock().unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_millis(200), fixture.check("1.2.3", 1000))
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            fixture.hits(),
            0,
            "a competing process never waits or starts another request"
        );
        drop(lock);
        assert!(fixture.check("1.2.3", 1000).await.is_some());
        assert_eq!(fixture.hits(), 1);
    }

    #[tokio::test]
    async fn notice_timeout_closes_the_request_and_keeps_reconnect_backoff() {
        let fixture = NoticeServer::start("200 OK", NOTICE_RELEASE).await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 1024];
            while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                let size = stream.read(&mut buffer).await.unwrap();
                assert_ne!(size, 0);
                request.extend_from_slice(&buffer[..size]);
                assert!(request.len() < 8192);
            }
            let closed = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut buffer))
                .await
                .unwrap();
            assert!(matches!(closed, Ok(0)) || closed.is_err(), "{closed:?}");
        });
        let client = reqwest::Client::builder()
            .no_proxy()
            .pool_max_idle_per_host(0)
            .timeout(Duration::from_millis(100))
            .build()
            .unwrap();
        let notice = tokio::time::timeout(
            Duration::from_secs(2),
            notice_at(client, &url, fixture.directory(), v("1.2.3"), 1000),
        )
        .await
        .unwrap();
        assert_eq!(notice, None);
        server.await.unwrap();
        assert_eq!(fixture.check("1.2.3", 1001).await, None);
        assert_eq!(fixture.hits(), 0);
        let directory =
            super::super::interface::history::SafeDirectory::at(fixture.directory()).unwrap();
        let _released = directory.lock().unwrap();
    }

    #[test]
    fn options_are_local_and_unambiguous() {
        assert_eq!(mode(&[]).unwrap(), Mode::Install);
        assert_eq!(mode(&["--check".into()]).unwrap(), Mode::Check);
        assert_eq!(mode(&["--help".into()]).unwrap(), Mode::Help);
        assert_eq!(mode(&["-h".into()]).unwrap(), Mode::Help);
        for args in [
            vec!["latest".into()],
            vec!["--force".into()],
            vec!["--check".into(), "--server".into()],
        ] {
            assert!(mode(&args).is_err());
        }
    }
    #[test]
    fn versions_compare_numerically_and_reject_untrusted_tags() {
        assert!(v("0.10.0") > v("v0.9.999"));
        assert!(v("1.0.0") > v("0.99.99"));
        assert_eq!(v("v0.9.4"), v("0.9.4"));
        for bad in [
            "",
            "1.2",
            "1.2.3.4",
            "01.2.3",
            "1.2.3-rc1",
            "1.2.3+build",
            "1.2.-3",
            "1.2.3/../x",
            "1.2.3\n",
            "1.2.18446744073709551616",
        ] {
            assert!(Version::parse(bad).is_err(), "{bad}");
        }
    }
    #[test]
    fn metadata_requires_a_published_stable_release() {
        assert_eq!(
            release_version(br#"{"tag_name":"v0.9.5","draft":false,"prerelease":false}"#).unwrap(),
            v("0.9.5")
        );
        for bad in [
            r#"{"tag_name":"v0.9.5","draft":true,"prerelease":false}"#,
            r#"{"tag_name":"v0.9.5","draft":false,"prerelease":true}"#,
            r#"{"tag_name":"v0.9.5-rc1","draft":false,"prerelease":false}"#,
            r#"{"tag_name":"v0.9.5"}"#,
        ] {
            assert!(release_version(bad.as_bytes()).is_err());
        }
    }
    #[tokio::test]
    async fn check_same_and_older_releases_never_touch_installation() {
        let absent = Path::new("/does-not-exist/ssmcp");
        for (local, remote, check, expected) in [
            ("0.9.4", "0.9.4", false, Outcome::Current),
            ("0.9.4", "0.6.3", false, Outcome::NewerInstalled),
            ("0.9.4", "0.9.5", true, Outcome::Available),
        ] {
            assert_eq!(
                update_to(v(local), v(remote), check, absent, None)
                    .await
                    .unwrap(),
                expected
            );
        }
    }
    #[test]
    fn managed_and_renamed_executables_are_protected() {
        for path in [
            "/usr/bin/ssmcp",
            "/home/test/.cargo/bin/ssmcp",
            "/nix/store/a/bin/ssmcp",
            "/opt/homebrew/Cellar/ssmcp/1/bin/ssmcp",
            "C:/Users/test/scoop/apps/ssmcp/current/ssmcp.exe",
        ] {
            assert!(
                package_manager_hint(Path::new(path), None).is_some(),
                "{path}"
            );
        }
        assert!(
            package_manager_hint(
                Path::new("/tools/cargo/bin/ssmcp"),
                Some(Path::new("/tools/cargo"))
            )
            .is_some()
        );
        assert!(package_manager_hint(Path::new("/home/test/.local/bin/ssmcp"), None).is_none());
        assert!(install_directory(Path::new("/tmp/renamed"), None).is_err());
    }
    #[test]
    fn private_staging_is_removed() {
        let parent = temporary();
        let directory;
        {
            let installer = Installer::prepare(&parent).unwrap();
            directory = installer.directory.clone();
            assert!(installer.script.is_file());
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(
                    std::fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
                    0o700
                );
            }
        }
        assert!(!directory.exists());
        std::fs::remove_dir(parent).unwrap();
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn installer_gets_literal_arguments_and_failures_preserve_original() {
        let parent = temporary();
        let directory = parent.join("bin with spaces and ' quotes");
        std::fs::create_dir(&directory).unwrap();
        std::fs::write(directory.join("ssmcp"), b"original").unwrap();
        let script = parent.join("install.sh");
        std::fs::write(&script,"test \"$1\" = --version && test \"$2\" = v0.9.5 && test \"$3\" = --dir || exit 9\nprintf '%s' \"$4\" > \"$4/args\"\nexit 23\n").unwrap();
        assert!(
            run_installer(&script, &directory, v("0.9.5"))
                .await
                .unwrap_err()
                .to_string()
                .contains("Verified installer failed")
        );
        assert_eq!(
            std::fs::read_to_string(directory.join("args")).unwrap(),
            directory.to_str().unwrap()
        );
        assert_eq!(std::fs::read(directory.join("ssmcp")).unwrap(), b"original");
        std::fs::remove_dir_all(parent).unwrap();
    }
    #[tokio::test]
    async fn http_failure_and_metadata_bounds_are_enforced() {
        for (status, body, declared) in [
            (
                "200 OK",
                r#"{"tag_name":"v0.9.5","draft":false,"prerelease":false}"#,
                None,
            ),
            ("503 Service Unavailable", "unavailable", None),
            ("200 OK", "{}", Some(MAX_METADATA + 1)),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = [0; 2048];
                let _ = stream.read(&mut request).await.unwrap();
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    declared.unwrap_or(body.len())
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            });
            let client = reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(3))
                .build()
                .unwrap();
            let result = latest_release(&client, &url).await;
            if status == "200 OK" && declared.is_none() {
                assert_eq!(result.unwrap(), v("0.9.5"));
            } else {
                assert!(result.is_err());
            }
            server.await.unwrap();
        }
    }
}
