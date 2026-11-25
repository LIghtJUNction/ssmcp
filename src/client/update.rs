//! Observational startup checks and explicit, checksum-verified self-update.
mod notice;
mod startup;
pub(super) mod terminal;
pub(super) use notice::{ConnectionVersions, report_connection};
pub(super) use startup::available_notice;
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

Update this installed client and check its independent official tool repository.
  --check     Check releases and fetch tool metadata without replacing the client
  -h, --help  Show this help

Downloads use the bundled installer, official SHA256SUMS and a version check.
Older releases never replace a newer client. Package-manager installations
must be updated with their original package manager.
Existing sessions keep their current executable until restarted.
Tool checks may initialize the offline Git seed and fetch objects/refs. Tool updates
only fast-forward a clean checkout; local edits and override repositories remain.
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
        let version = crate::wire::versions::Version::parse(value)
            .filter(|version| version.stable() && !value.contains('+'))
            .ok_or("release version must be stable MAJOR.MINOR.PATCH")?;
        Ok(Self(version.core))
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

async fn release_metadata(client: &reqwest::Client, url: &str) -> crate::Result<Vec<u8>> {
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
    Ok(body)
}

async fn latest_release(client: &reqwest::Client, url: &str) -> crate::Result<Version> {
    release_version(&release_metadata(client, url).await?)
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
    if cfg!(target_os = "android") {
        return android_update(mode).await;
    }
    desktop_update(mode).await
}

async fn android_update(mode: Mode) -> crate::Result<()> {
    const MAGISK_ZIP: &str = "ssmcp-magisk-arm64.zip";
    let executable = std::env::current_exe()?;
    let module_entry = [
        "/data/adb/modules/ssmcp/bin/ssmcp",
        "/data/adb/modules_update/ssmcp/bin/ssmcp",
    ]
    .iter()
    .any(|path| super::reload::same_executable(Path::new(path), &executable).unwrap_or(false));
    if !module_entry {
        return Err("For the supported Termux installation, run ssmcp update from the ssmcp-debian proot guest.".into());
    }
    let client = crate::http_client::builder()
        .user_agent(concat!("ssmcp/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(20))
        .build()?;
    let bytes = release_metadata(&client, API).await?;
    let latest = release_version(&bytes)?;
    let metadata: serde_json::Value = serde_json::from_slice(&bytes)?;
    let published = metadata["assets"].as_array().is_some_and(|assets| {
        assets
            .iter()
            .any(|asset| asset["name"] == MAGISK_ZIP && asset["state"] == "uploaded")
    });
    if !published {
        return Err(format!("{} has no published Magisk ZIP.", latest.tag()).into());
    }
    let current = Version::parse(env!("CARGO_PKG_VERSION"))?;
    if latest <= current {
        if latest == current {
            println!("Installed {} is current for Magisk.", current.tag());
        } else {
            println!(
                "Installed {} is newer than the published Magisk release {}.",
                current.tag(),
                latest.tag()
            );
        }
        return Ok(());
    }
    println!(
        "Magisk update {} available: https://github.com/LIghtJUNction/ssmcp/releases/download/{}/{MAGISK_ZIP}",
        latest.tag(),
        latest.tag()
    );
    if mode == Mode::Install {
        println!("Flash the ZIP in Magisk and reboot. Your ssmcp.rc remains in /data/adb/ssmcp.");
    }
    Ok(())
}

async fn desktop_update(mode: Mode) -> crate::Result<()> {
    println!("Checking the official SSMCP tool repository…");
    let tool_update =
        crate::tool_repository::update(&std::env::current_dir()?, mode == Mode::Install).await;
    match &tool_update {
        Ok(report) => println!("Tool repository: {}", serde_json::to_string(report)?),
        Err(error) => eprintln!("Tool repository check failed: {error}"),
    }
    let current = Version::parse(env!("CARGO_PKG_VERSION"))?;
    println!("Checking the latest stable ssmcp release…");
    let client = crate::http_client::builder()
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
    tool_update.map(|_| ()).map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }
    pub(super) fn temporary() -> PathBuf {
        let mut nonce = [0; 16];
        getrandom::fill(&mut nonce).unwrap();
        let path = std::env::temp_dir().join(format!(
            "ssmcp-update-test-{:x}",
            u128::from_le_bytes(nonce)
        ));
        std::fs::create_dir(&path).unwrap();
        path
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
            let client = crate::http_client::builder()
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
